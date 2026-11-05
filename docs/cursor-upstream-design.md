# Cursor subscription upstream (R-8, R-9)

Status: design only. The subscription crate already signs in, imports, refreshes, lists models, and reads quota. `Subscription::upstream` still returns unsupported (`plugins/subscriptions/cursor/src/lib.rs:365-372`). This document is the contract for serving a model call on a Cursor account.

The wire lives in the reference's JavaScript plugin (`packages/cursor/index.mjs`, `native-session.mjs`, and the tests named below). The Go provider (`internal/provider/cursor.go`) is sign-in, keychain, and catalog context only. It does not open `AgentService/Run`. Where the two disagree, the JavaScript plugin is the wire contract, and the Go file is the keychain contract.

Vendor names stay inside `plugins/subscriptions/cursor`. Kernel and router changes in this document are generic HTTP. Plugins still depend only on `hofund-plugin-api` and `hofund-plugin-sdk` (`docs/plugins/boundary.md`).

## 0. Decision, and where it is weak

Ship a stateless Run per `UpstreamProvider::execute`. Part 1 (R-8) returns streamed text on a real session: tokens, agent host, HTTP/2 Connect with an HTTP/1.1 fallback, blob fetch, heartbeats, and a refusal of every exec. Part 2 (R-9) adds caller tool calls, reasoning, usage, Max Mode, and error kinds the router already rests on. Native-session retention is not part of either item.

This will be wrong if we treat the reference's retained Run as the baseline. That path is an optimization on top of a stateless decoder (`index.mjs` `answerOf`, around 3360-3376; `native-session.mjs` `NativeSessionPool`). A lost owner rebuilds history on a fresh Run. Hofund has no process-wide owner table, and `Request` has no session id (`crates/kernel/ir/src/request.rs:16-39`). Building retention first would block text on the least stable part of the reference (15-minute lifetime, 120-second result wait, continuity fallback). The stateless Run is the behavior a restarted process already has.

This will also be wrong if R-8 silently drops tools or images. A tool-bearing request that is sent with an empty tool list can still make the model call a tool the client never sees. R-8 must refuse execs so the Run can finish, and it must not emit `StreamEvent` tool calls. R-9 is what turns those execs into caller tool calls. An image in R-8 is a hard error before the Run starts, so a vision prompt is not answered as if the picture had been sent.

## 1. Wire protocol

### 1.1 Connect framing

`frame` / `frames` (`index.mjs:1616-1639`).

Each frame is a 5-byte head plus a payload. Byte 0 is flags. Bytes 1-4 are a big-endian payload length. The payload is one protobuf message. Flag bit 0 (`flags & 1`) means the payload is gzip. Flag bit 1 (`flags & 2`) means this is the stream trailer, and the payload is JSON, not protobuf. A payload longer than 64 MiB is a malformed stream. A leftover byte after the body ends is `unexpected EOF`. A malformed tail inside a message is dropped, not fatal (`fields`, `index.mjs:1511-1560`): an unknown wire type, a truncated length, or a truncated fixed field ends the walk and keeps the fields already parsed.

The trailer JSON is what `failure` parses (`index.mjs:1650-1665`): either `{code, message, details}` or the same object under `error`. The useful text is `details[].debug.details.title` plus `detail` when that pair is more than `":"`.

Hofund does not have an inflate helper yet (`plugin-sdk/Cargo.toml` has no `flate2` entry). The plugin cannot take a new dependency (`docs/plugins/boundary.md`). R-8 adds `hofund_plugin_sdk::inflate::gzip` backed by `flate2` 1.1.10 (`Cargo.lock`) with `default-features = false` and the pure-Rust backend so it shares `miniz_oxide` 0.9.1 with the rest of the tree. `miniz_oxide` 0.8.9 is already pulled by `png` for the menu-bar icons (`deny.toml:57` skips that duplicate until the icon stack moves to 0.9; adding SDK `flate2` must not add a new skip). The helper is generic. It does not mention Cursor. A frame with the gzip bit set and a body `inflate` rejects is a decode error for that Run, not a retry on the other transport.

### 1.2 HTTP/2 Run

`openH2` (`index.mjs:2388-2423`) and the headers in `runOnce` (`index.mjs:2840-2886`).

`POST {agent-origin}/agent.v1.AgentService/Run`.

Headers on that request:

| Header | Value | Source |
|---|---|---|
| `content-type` | `application/connect+proto` | `runOnce` |
| `connect-protocol-version` | `1` | `runOnce` |
| `authorization` | `Bearer {access token}` | `runOnce` |
| `x-cursor-client-type` | `cli` | `runOnce`; comment: without it the server adds its own prompt |
| `x-cursor-client-version` | `cli-` plus the installed version, else `cli-2026.09.23-86fc751` | `clientVersion`, `index.mjs:88-109`; `VERSION_FALLBACK` at line 48 |
| `x-ghost-mode` | `true` | privacy mode |
| `x-request-id` | a new UUID for this Run | `runOnce` |
| `x-cursor-team-id` | decimal team id, only when section 5.4 says one exists | `teamHeader`, `index.mjs:173-176` |
| `x-cursor-agent-allowed-tools` | `mcp_tool_call,get_mcp_tools_tool_call` | `runOnce` |

The client waits up to 15 seconds for response headers (`H2.wait`, `index.mjs:2362-2364`). Hofund writes the first client frame only after a 2xx head on the duplex path (section 3.3). The reference's `runOnce` sends the opening frame and starts the 5-second heartbeat first, then reads the status and, on failure, drains up to 1 MiB for `failure` (`index.mjs:2886-2895`). Match the reference on HTTP/1.1 fallback (RunSSE may fail before any append). On HTTP/2, prefer not writing user payload until 2xx so a refused head never ships conversation bytes; document that choice in the adopt-or-redesign table.

A head outside 200-299 on HTTP/2 (after the reference's early write) is read to completion (capped at 1 MiB) and passed to `failure`. A 2xx head is a Connect frame stream in both directions.

Client heartbeat: every 5 seconds, one frame whose message is field 7, length-delimited, empty (`runOnce`, line 2886). That heartbeat is not a KV reply. Field 7, wire type 2, tags as `0x3a`. KV replies tag as `0x1a` (`kvReply`, `index.mjs:2440-2442`).

The Go files do not build these headers. Hofund's dashboard client already sends the version, `cli`, and ghost mode, and it does not send the team id (`plugins/subscriptions/cursor/src/connect.rs:16-28`). The version string there is the fallback with the `cli-` prefix (`endpoints.rs:19`). R-8 keeps that fallback and prefers an installed `cursor-agent` version directory whose name matches `^\d{4}\.\d{2}\.\d{2}-[0-9a-f]+$`, the same way `clientVersion` does.

### 1.3 Agent host

`agentURL` (`index.mjs:2310-2331`).

Default when config cannot be read: `https://agentn.global.api5.cursor.sh` (`index.mjs:46`).

Otherwise `POST https://api2.cursor.sh/aiserver.v1.ServerConfigService/GetServerConfig` with a JSON body `{}`, Connect JSON headers from `headersFor` (`index.mjs:275-285`, including the team header), timeout 10 seconds. Take the first of `agentUrlConfig.agentUrl`, `agentUrlConfig.agentnUrl` that parses as `http:` or `https:` and has a host. Strip trailing slashes.

Cache key is the SHA-256 hex of the access token, not the token. A listed URL is kept until a forced refresh. An unlisted fallback is kept for 60 seconds.

Region retry (`answerOf`, `index.mjs:3380-3390`): if the Run's error message contains `region` (case-insensitive, `regional` at line 1644), clear the HTTP/1.1 memory, refetch config with `fresh`, and run once more when the URL changed or the failed attempt was HTTP/1.1. One retry. The HTTP/1.1 path always talks to `api2.cursor.sh`, so a regional account on HTTP/1.1 must be sent back to HTTP/2 on the regional agent host.

Hofund's router `REGION_WORDS` (`crates/kernel/router/src/decision.rs:83-86`) does not match a bare "region". The plugin does this retry itself, before it returns to the router. After the second failure, the error in section 6 is what the router sees.

### 1.4 HTTP/2-unavailable memory

`open` and `h2Blocked` (`index.mjs:2343-2382`). `H2.retryAfter` is 10 minutes (line 2364).

Remember HTTP/2 as unavailable only when the failure is one of:

- the 15-second head timer (`h2Timeout`)
- an error whose code starts with `ERR_HTTP2`, except `ERR_HTTP2_STREAM_ERROR`
- a message matching `h2 is not supported`, `alpn`, `nghttp2`, or `protocol error`

Do not remember it for a connection failure (`ECONNREFUSED`, `ENOTFOUND`, `EAI_AGAIN`, `ENETUNREACH`, `EHOSTUNREACH`, `ECONNRESET`, `ETIMEDOUT`, `ECONNABORTED`, `EPIPE`) or for a stream the server reset or refused before the head (`http1.test.mjs` covers stall, refuse, and the 10-minute stickiness). The memory is process-global in the reference, not per account: one blocked proxy affects every account. Match that. A region retry clears it.

The plugin owns this memory. The kernel must not store it. The plugin must not sniff raw `reqwest` strings. Section 3 gives the kernel error classes the plugin switches on.

### 1.5 HTTP/1.1 fallback

`openH1` (`index.mjs:2426-2583`). Tests: `http1.test.mjs`, `native-http1.test.mjs`.

Used when the memory is active, or when this Run's HTTP/2 open failed as above. Base is always `https://api2.cursor.sh`, not the agent host.

Down: `POST /agent.v1.AgentService/RunSSE`. Body is one Connect frame of a message with field 1 set to the same `x-request-id`. `content-type: application/connect+proto`. Extra headers: `accept-encoding: identity`, `x-cursor-streaming: true`. The response body is the same Connect frame stream as HTTP/2. A non-2xx becomes a synthetic end frame (flag byte 2) whose JSON is `{error: {code, message}}`. `connectCode` (`index.mjs:2447-2448`) maps 400, 401, 403, and 429 to `invalid_argument`, `unauthenticated`, `permission_denied`, and `resource_exhausted`. Anything else is `unavailable`.

Up: each client frame's payload (the 5-byte head stripped) is `POST /aiserver.v1.BidiService/BidiAppend`. Headers are the same set as the Run (`authorization`, `x-request-id`, `x-cursor-client-version`, `x-cursor-client-type`, `x-ghost-mode`, optional `x-cursor-team-id`, `x-cursor-agent-allowed-tools`, `connect-protocol-version`), plus `accept-encoding: identity` and `x-cursor-streaming: true` (`openH1`, `index.mjs:2465`), then `content-type: application/proto` overrides the Run's `application/connect+proto` on each append (`index.mjs:2528-2531`). The protobuf body is:

- field 1, string: the payload hex-encoded
- field 2, bytes: a message whose field 1 is the request id
- field 3, varint: sequence number, starting at 0, in write order

`BidiAppend` calls stay ordered. Up to 16 may be in flight (`H1.atOnce`). A message `kvReply` accepts (first payload byte `0x1a`, field 3 length-delimited) may sit in that window. Any other message waits until every earlier append has completed, goes alone, and is acknowledged before the next write. A native Run sends only KV replies in the window; a tool result does not (`index.mjs:2428-2433`). R-8 has the same split: the opening Run request and exec refusals are alone; blob replies pipeline.

Deadline: 60 seconds plus `Math.ceil(body.length / 65536) * 1000` milliseconds, where `body` is the full protobuf built for that append (`index.mjs:2524-2525`, `2528-2532`). One byte of payload therefore gets 61 seconds. A timeout ends the Run with Connect code `deadline_exceeded`. A transport failure ends it with `unavailable`. The RunSSE read and the appends share one cancel flag.

### 1.6 Messages the codec must read and write

The reference codec is field-number based (`class PB`, `index.mjs:1471-1613`). Names below are the comments in that file, not a checked-in `.proto`. Implement those numbers. Unknown fields are skipped and counted so an empty-reply diagnostic can name them (`emptyWhy`, `index.mjs:3213-3221`).

Client message (one unframed payload, then framed on the wire):

| Field | Wire | Role | Built in |
|---|---|---|---|
| 1 | bytes | `AgentRunRequest`, the first payload | `buildRun` returns `pb().bytes(1, rr)` (`index.mjs:2299`) |
| 2 | bytes | exec result: field 1 varint id, field 15 string exec id, then the result at the same field number the server used | `exec` `answer`, `index.mjs:3252-3254` |
| 3 | bytes | KV reply on client field 3: inner field 1 = KV id; inner field 2 = result bytes (found: result field 1 = blob; missing: result field 2 with inner field 1 = `"blob not found"`); store ack: inner field 3 empty | `decode` case 4, `index.mjs:3154-3167` |
| 5 | bytes | close an exec: inner field 1 is a message with field 1 = id. A refusal is inner field 2: field 1 = id, field 2 = `"not available"` | `closeExec` line 3019; `unavailable` lines 3256-3260 |
| 7 | bytes | client heartbeat, empty | `runOnce` line 2886 |

`AgentRunRequest` (`buildRun`, `index.mjs:2262-2300`):

| Field | Contents |
|---|---|
| 1 | conversation state |
| 2 | action: field 2 wraps field 2 again, which wraps the request context (`pb().bytes(2, pb().bytes(2, rc))`, `index.mjs:2282-2294`) |
| 3 | model details: field 1, 3, and 4 are the model id string; field 7 varint 1 when Max Mode (R-9) |
| 4 | MCP wrapper: field 1 repeated tool definitions |
| 5 | conversation id string |
| 9 | requested model: field 1 model id; field 2 varint 1 when Max Mode (R-9); field 3 repeated `{field 1 id, field 2 value}` parameters (R-9 effort from the picker) |
| 19 | varint 1, inline images |

Conversation state: field 1 repeated SHA-256 ids of transcript blobs; field 8 the user-turn blob id; field 10 varint 1; field 22 `"cli"`.

User turn blob: field 1 is the last user text, field 2 a message id, field 4 varint 1 (agent mode). The state points at a blob of `field 1 = (field 1 = that user message's blob id, field 10 = the same message id)`.

Request context (`env` and `toolDef`, `index.mjs:1876-1883`): field 4 is the environment (field 1 OS name, field 2 temp directory, field 10 `"UTC"`). Field 7 repeated is an MCP tool definition: field 1 name, field 2 description, field 3 a `google.protobuf.Value` of the JSON schema (`pbValue`, lines 1568-1583), field 4 namespace `"tools"`, field 5 name, field 6 schema JSON text.

Transcript blobs are UTF-8 JSON, not protobuf. `conversation` (`index.mjs:1794-1870`) writes AI SDK-shaped messages. Section 4 maps hofund `Request` onto that JSON. Blob ids are SHA-256 of those bytes. The server asks for them by the raw 32-byte digest, hex-looked-up (`decode` case 4).

Server message, top level (`decode`, `index.mjs:3069-3173`):

| Field | Meaning |
|---|---|
| 1 | interaction update, oneof inside |
| 2 | exec request |
| 3 | checkpoint |
| 4 | KV request |

Interaction update (`kindOf` names in `native-session.mjs:72-77`, read in `decode`):

| Field | R-8 | R-9 |
|---|---|---|
| 1 | text delta, inner field 1 string. Emit text. | same |
| 2 | count only | tool call started. Part of the step's call set. |
| 3 | count only | tool call completed. A completion with no matching start was finished by the server (`stall-550.test.mjs`). |
| 4 | ignore the bytes in R-8, still reset the stall clock | thinking delta, inner field 1 string |
| 5 | ignore | thinking completed |
| 7 | count only | partial tool call |
| 8 | ignore | token delta, not a usage total |
| 13 | heartbeat. `beat` (`index.mjs:3043-3049`): every top-level field is 1 and every inner field is 13. Does not reset the quiet clock. | same |
| 14 | turn end. End the text turn. | usage, `usageOf` (`index.mjs:1941-1944`): field 1 input, 2 output, 3 cache read, 4 cache write, 5 reasoning. Input reported to the client is `max(input - cache_read - cache_write, 0)` plus the two cache counters added back into the prompt total. |
| 27 | ignore | tool-call count for the step, inner field 1 |

Exec (`exec`, `index.mjs:3224-3298`):

| Field | Action |
|---|---|
| 1 | uint32 id. Omitted means 0. |
| 10 | request context. Reply: exec field 10 result is field 1 → field 1 → innermost field 4 = environment (`index.mjs:3292-3294`). Then `closeExec`. Not a caller tool. |
| 11 | MCP arguments. R-8 refuses. R-9 maps to a caller tool. Inner field 1 may be a namespaced name, field 2 is argument entries (`field 1` key, `field 2` a protobuf Value), field 3 is the call id, field 5 is the tool name. |
| 15 | exec id string, copied into the result |
| 36 | MCP tool list. Reply: field 1 → field 1 with inner fields 1 and 2 = `"tools"`, field 7 = `"connected"`, field 5 repeated tool defs (empty when R-8 has no tools; not a bare empty message) (`index.mjs:3285-3290`). R-9 repeats the request's tools. Then `closeExec`. |

Anything else, including shell and file tools, is refused with `"not available"`, then `closeExec` (client field 5 → field 2 → field 1 = id, field 2 = `"not available"`, `index.mjs:3256-3260`, `3019`). The model is allowed to continue. The plugin never runs those tools (`README.md` "Cursor's own tools are never run").

Checkpoint field 3: R-8 ignores it apart from stall timing. R-9 ends a tool step when a count has arrived, at least one call was handed to the caller, and `pendingCalls` equals that handed count (`decode`, `index.mjs:3142-3150`; `stall-550.test.mjs`).

KV field 4: field 1 is the request id; field 2 asks for a blob (inner field 1 is the digest); field 3 asks the client to acknowledge a stored blob. Client replies use top-level field 3 as in the table above (`index.mjs:3161-3167`).

`google.protobuf.Value` field numbers are the standard ones: 1 null, 2 double, 3 string, 4 bool, 5 struct, 6 list (`pbValue` / `pbAny`).

### 1.7 Stall and other clocks

From `native-session.mjs` `NATIVE_LIMITS` (lines 8-47) and `Quiet` (lines 93-231), and from `H1` / `H2`:

| Clock | Duration | What it measures |
|---|---|---|
| HTTP/2 head | 15 s | time to response headers. Failure can set the 10-minute memory. |
| HTTP/2 memory | 10 min | later Runs skip HTTP/2 |
| Append | 60 s + ceil(protobuf body / 65536) s | one `BidiAppend` (`index.mjs:2524-2525`) |
| Idle | 60 s | no server frame at all. `Quiet.fire`. |
| Quiet | 240 s | only heartbeats. Accumulated wait, not wall time while the plugin is busy, and not time parked on the caller. |
| Client heartbeat | 5 s | our field-7 frames |
| Config negative cache | 60 s | unlisted agent URL |
| Model list | 10 min | not on the Run path in R-8 |
| Tool count wait | 60 s | R-9 only. After at least one caller call, if no count has arrived and nothing is still streaming a call, heartbeats do not extend the wait (`decode` `read`, `index.mjs:3020-3057`; `countWait` in `native-session.mjs:19-24`). |
| Outbound `writeWait` | 30 s | native retention only: KV reply waits for queue room (`native-session.mjs:36`). Deferred. |
| Outbound `drain` | 6 s | native retention only: delay after turn end before `complete()` (`native-session.mjs:25`, `1236-1238`). Deferred. |

`Quiet` resets its heartbeat tally on any non-heartbeat frame. The error is HTTP 502 and includes the last frame kind. A checkpoint names its index. Tests shrink these limits; production uses the table.

A Run the caller cancels is not a stall and does not set the HTTP/2 memory (`cancelledOpen`, status 410 in the reference, `index.mjs:2344`). Hofund uses `PluginErrorKind::Cancelled`, which the router does not rest (`cooldown.rs` `failure_kind`, lines 1483-1486).

## 2. Protobuf approach

Use a hand-written codec in the Cursor plugin. Do not add `prost`.

Weights:

| | Hand-written codec | `prost` plus a `.proto` |
|---|---|---|
| What it must match | `PB`, `fields`, `pbValue`, `frame` (`index.mjs:1471-1639`), including "drop a malformed tail" | A schema we do not have. The reference never compiled one. Field numbers are the spec. |
| Dependencies | None beyond `flate2` in the SDK for the frame gzip bit, and `ring` (already in the SDK) for SHA-256 blob ids | `prost` runtime. A build script is forbidden on a plugin (`docs/plugins/boundary.md`, `xtask/src/boundary.rs`). Checked-in generated code still needs the runtime crate. |
| Boundary | Legal. The plugin may depend on the SDK, and the SDK grows one inflate function. | Illegal as a direct plugin dependency. Putting `prost` in the SDK compiles it into every plugin for one vendor's partial schema, and it puts field numbers in a crate that must stay vendor-neutral. |
| `cargo-deny` | SDK `flate2` with `rust_backend` reuses `miniz_oxide` 0.9.1; 0.8.9 stays only on the `png` path until that dependency upgrades. | `prost-types` and any older `prost` pulled by a future crate trip `multiple-versions = "deny"`. |
| Build time | A few hundred lines of plain Rust. No `protoc`. | A plugin cannot run `protoc`. Generating elsewhere and checking the output in still needs a human to regenerate when a field is discovered. |
| Maintenance | New fields are an explicit match arm plus a golden frame. Unknown fields already pass through the skip path. | Generated structs invite sending fields the reference never sends. `upstream-words.test.mjs` forbids the reference product name in any outbound byte. A generated catalog is how that name leaks back in. |
| Numeric range | Decode varints as `u64`. The reference uses JavaScript numbers and goes wrong past 2^53. Exec ids are uint32 (`exec`, `index.mjs:3228-3233`). A test locks a varint above 2^53 so we do not copy that bug. | `prost` would also use `u64`, then we would still hand-check the subset. |

Attack on the hand-written codec: it will drift. The defense is golden frames taken from `native-test-wire.mjs` shapes used by `stall-550.test.mjs` (text, thinking, heartbeat, exec, count, checkpoint, get-blob, set-blob), round-tripped in Rust, plus a fuzz-sized truncated buffer that must match `fields`' "return what you have" rule. Do not invent packed repeated fields or groups. The reference reader rejects wire type other than 0, 1, 2, and 5 by stopping.

Namespace on the wire is the literal `tools` (`index.mjs:1749`). Outbound bytes, prompts, and tool definitions use only that namespace (`upstream-words.test.mjs`). The reference also accepts one historical MCP namespace when decoding names (`NAMESPACES`, `index.mjs:1749-1750`, `bareName`); that string is the upstream project's name and must not appear in plugin source, tests, or this file (`xtask/tests/workspace_invariants.rs` `inspiration_name_is_not_used_as_branding`; `docs/plugins/models.md` on reference-named wire fields). Accepting that alias on read is deferred. Until then, when exec field 5 is empty and field 1 has no `tools-` prefix, leave the tool name unchanged (`index.mjs:3247`). Do not add a constant table or golden frame containing the alias.

## 3. Transport inside hofund

### 3.1 What exists

`HttpHandle::send` takes `http::Request<Body>` and returns `http::Response<BodyStream>` (`crates/kernel/plugin-api/src/host.rs:36-52`). `Body` is a finished byte buffer (`body.rs:11-16`). `BodyStream` is the response only. `HttpBackend::send` has the same shape (`kernel.rs:47-55`). There is no second write after headers, no HTTP version pin, and no duplex split.

`Call::open` (`plugin-sdk/src/transport.rs:290-296`) reads the body and turns any status ≥ 400 into the same `PluginError` as `Call::send` (`transport.rs:217-221`), which also consumes the body. That matches unary Connect JSON. It cannot implement the HTTP/1.1 Run path: on RunSSE or `BidiAppend` failure the reference reads the HTTP body itself, builds a synthetic Connect end frame (flag byte 2) whose JSON is `{error: {code, message}}`, and maps bare HTTP statuses through `connectCode` (`index.mjs:2511-2517`, `2447-2448`). Hofund must do the same on the plugin side.

The pooled client is `reqwest` 0.12 with `default-features = false` and only `rustls-tls-native-roots` (`crates/kernel/plugin-host/Cargo.toml:21`). The `http2` and `stream` features are off, so today's client does not speak HTTP/2. `HttpLimits::default` uses a 10-second connect budget and a 60-second idle read budget (`pool.rs:23-24`, `66-72`). The read budget resets after each successful read. It is process-wide.

Attack this if someone enables `http2` on that client and stops there. Every plugin would then negotiate HTTP/2. Several of those servers sit behind proxies that fail ALPN. The Cursor fallback exists because of that failure. The unary pool must stay HTTP/1.1 even after the crate feature is turned on.

### 3.2 Kernel extension

Add one method. No vendor names, no Cursor headers, no Connect framing in the kernel.

```rust
// plugin-api, beside HttpHandle
pub enum HttpVersionPin { Http1, Http2 }

pub struct DuplexRequest { /* method, uri, headers, sensitive headers, pin, header_timeout */ }

pub struct DuplexSession {
    // status, response headers
    // reader: BodyStream
    // writer: DuplexWriter   // write(Bytes), finish()
}
```

`HttpHandle::open_duplex` and `HttpBackend::open_duplex`. `DuplexSession` is not `Clone`. Dropping it, or dropping either half after `into_split`, aborts the call. The writer and the reader are `Send`. The plugin polls both on the `execute` task. It must not move the writer onto `SchedulerHandle::run_once`: that future outlives the request (`host.rs:350-361`) and would keep appending after the caller cancelled.

`plugin-host` enables `reqwest` features `http2` and `stream`. The existing pooled builder sets HTTP/1.1 only, so unary `send` keeps today's behavior. A second client, keyed by the same `HttpProfile` (proxy and TLS verify bit, `pool.rs:26-58`) plus a duplex marker, allows HTTP/2. `pool_max_idle_per_host` for that client is 0. The reference dials a session per Run and destroys it (`openH2` `session.destroy`, `runOnce` close). Reusing a dead HTTP/2 connection is how a single protocol error would stick past the plugin's own 10-minute memory. The cost is a handshake per Run. Accept it.

URL rules:

- `https`: ALPN. This is the production path.
- `http`: HTTP/2 prior knowledge, so a loopback test can run without a certificate. Production agent URLs are `https`. A cleartext prior-knowledge dial to a non-loopback host is a `Config` error, so a proxy URL cannot be downgraded by accident.

Header wait is a timeout around the dial-and-headers future, defaulted by the caller. R-8 passes 15 seconds. It is not the process-wide 60-second read budget.

Idle read budget on the duplex client is 5 minutes. The plugin's 60-second idle and 240-second quiet clocks must win while frames still arrive. A socket that is silent for 5 minutes is a backstop and surfaces as `Interrupted`. The unary pool's 60-second budget stays. RunSSE uses that pool: server heartbeats reset it, and a truly silent Run hits 60 seconds, which matches the idle clock.

Proxy: the duplex client uses `HttpProfile`'s proxy, the same `reqwest::Proxy::all` the pool uses (`pool.rs:203-206`). The plugin does not read proxy environment variables and does not build a client.

Error classes, stable prefixes the plugin matches. These strings are the contract. They must not contain a vendor name. They are returned only to the plugin; the plugin translates a failed open into a fallback, and the router does not see the HTTP/2 attempt when HTTP/1.1 then succeeds.

| Situation | `PluginError` | Plugin treats it as |
|---|---|---|
| DNS, TCP, TLS, or proxy tunnel failed before HTTP/2 | `Connect` | do not set the 10-minute memory |
| Headers did not arrive within the header timeout | `Timeout`, message prefix `duplex headers timed out` | set the memory |
| ALPN or the peer refused HTTP/2 | `Transport`, message prefix `http2 unavailable` | set the memory |
| Stream reset or refused before headers | `Transport`, message prefix `stream refused` | fall back for this Run only |
| Caller dropped the session | `Cancelled` | do not set the memory |
| Body failed after a 2xx head | `Interrupted` | the Run failed; do not flip the memory |

Attack on prefixes: a future edit that rewords them silently disables the fallback. A plugin-host unit test asserts the prefixes. The Cursor plugin tests assert it switches on the kind and the prefix, not on a substring of a `reqwest` error.

`PipedChild` has no exit status (`plugin-host/src/piped.rs:170-206`). The keychain fix in section 5 needs `exit_code()` after stdout reaches EOF. Drop still kills a child the caller abandoned. That method is generic.

### 3.3 What the plugin does with it

HTTP/2 Run: `open_duplex` with `HttpVersionPin::Http2`, write the first frame only after a 2xx head, then interleave writes and reads. HTTP/1.1 RunSSE: `Call::open_raw` on the HTTP/1.1 pool with `attempts = 1`, so a non-2xx status returns an unread body the plugin caps and maps like `failed` above. Stream the 2xx body. Each `BidiAppend`: `Call::fetch` with `attempts = 1`, then classify `!res.ok` locally. With `attempts = 1`, neither call retries, including `PluginErrorKind::Connect` (`transport.rs:312-327`: the retry loop never runs a second attempt). Keep attempts at 1 so a POST that may have reached the server is never sent twice. Do not use `Call::open` or `Call::send` on either HTTP/1.1 Run leg. Concurrency of 16 is a `FuturesUnordered` polled on the execute task. `futures-util` is not a plugin dependency. The SDK re-exports the small piece the plugin needs (`FuturesUnordered` and a two-future race). It does not become a general runtime.

Attack: sixteen blocking `Call::fetch` futures inside one task are fine only if each one yields. `Call::fetch` is async and uses the host client, so they will. A synchronous inflate of a 64 MiB frame on that task will stall the append window and the stall clock. Cap a frame at 64 MiB, inflate with a size cap, and treat a larger frame as malformed the way `frames` does.

## 4. Request mapping

`UpstreamProvider::execute` receives canonical `hofund_ir::Request` (`plugin-api/src/upstream.rs:401-416`). The gateway has already decoded Chat Completions, Responses, or Messages. The Cursor plugin does not parse those JSON bodies and does not branch on `inbound` except to know that extension bags were stripped when the inbound protocol is not the upstream protocol. There is no Cursor protocol id. Extensions on the request are not a session key. `prompt_cache_key` is not stored by the codecs.

### 4.1 Shared mapping

System and developer text are joined, in order, into one system blob. User, assistant, and tool messages follow `conversation` (`index.mjs:1794-1870`):

- Assistant text and, in R-9, thinking stay separate parts. Thinking must not be concatenated into text.
- Consecutive assistant messages are joined when the earlier one has tool calls (`joinSplitCalls`, `index.mjs:1704-1716`), so a split parallel tool turn is one assistant blob.
- A tool call with no result becomes an error result whose text is `Tool use was interrupted and did not produce a result.` (`NO_RESULT`, `index.mjs:52`).
- A data-URL or base64 image becomes an image part `{type:"image", mimeType, image:{__type:"Uint8Array", hex}}`. An image URL becomes the text placeholder `[attachment image: {url}]`. A non-image file becomes `[attachment {name}]`.
- The last user text is the user-turn string in `buildRun`. An empty conversation uses `"."` (`conversation`, line 1799).

Conversation id: a new UUID per `execute`. The reference does that when nothing names the session (`conversationID` returns `""` when there is no cache key and no session, `index.mjs:1924-1932`; `runOnce` then uses `randomUUID`, line 2833). Hofund `Request` cannot name the session. Hashing the first user message alone would merge unrelated `"hi"` turns, which is the case that function refuses. Prompt caching that depends on a stable conversation id is deferred until the IR carries a caller-supplied key. Do not add that field in R-8 or R-9.

Model id: R-8 sends `request.model` as model details and requested model, including the literal `auto`. It does not call the picker. The full picker (`request` / `pick` / `modelsOf` / `fromCatalog`, `index.mjs:1176-1217` and `1031-1150`) is a catalog project of its own: usable models, hidden Anthropic families, context-size ids, and fast variants. Hofund's `list_models` is `cursor-agent models` text, not `AvailableModels`.

`GenerationParams::effort` exists (`crates/kernel/ir/src/params.rs:37-42`, `effort.rs`). `service_tier` does not exist on `Request`. R-8 sends no effort parameters, no Max Mode bits on fields 3 or 9, and does not send `fast`. R-9 passes effort when the caller set it, using per-model level lists from the picker (`fitEffort`, `index.mjs:1245-1258`, `LEVEL_RANK`, line 862). Until the picker lands, defer effort parameters with the picker rather than inventing level lists in the plugin. Max Mode is independent of the picker (section 8, R-9). `Effort::None` is thinking off for a Claude-shaped id, matching `thinking=false` in `pick` (lines 1186-1188). Fast stays off until the IR can say it. Sending `fast=true` by default would bill a more expensive variant (`README.md` on `service_tier`).

Sampling parameters (`temperature`, `top_p`, penalties, stop, seed, `max_tokens`) have nowhere to go on this wire. They are ignored. Document that in the comparison table. Do not fail the request because of them: Chat clients send defaults.

### 4.2 Part 1, R-8, what a successful call returns

Tokens and session: section 5. Then one stateless Run.

The stream is `Completion::Stream` of:

1. `MessageStart` with a generated id and the requested model.
2. `ContentBlockStart` text index 0, then `TextDelta` for each interaction text field.
3. On a clean turn end with any text: `MessageDelta` with `Stop { reason: StopReason::EndTurn }`, `ContentBlockStop`, `MessageStop`.
4. Usage is omitted in R-8 even if field 14 arrives. The turn end is only a delimiter. Guessing usage here is how a client treats a made-up prompt size as a full context window.

Before returning `Ok`, drive the Run until the first text delta or a terminal failure. A region failure retries inside that loop (section 1.3). The router failovers only before the first byte (`decision.rs` `allows`). An internal retry after a yielded delta cannot be taken back.

KV fetches, client heartbeats, request-context execs, and refusals of every other exec, including MCP, are part of R-8. Without them a text Run stalls or dies on the first server question.

R-8 requests that contain an image or a tool (or a tool result) return `PluginError::status_error(404, ...)` with `without_cooldown`, and with wording that `too_long` does not match. Use only the fixed phrases "tools are not served" and "images are not served". `Unsupported` does not fail over (`allows` returns false for every kind other than connect-class, `Status`, and `Quota`, `decision.rs:171-178`). Status 404 is in the default failover set (`config` test `default_failover_lists_auth_and_every_server_status`, `crates/kernel/config/src/lib.rs:2712-2717`); 400 is not, and a 400 without the shape or wrong-endpoint word lists would not try another member (`allows`, `decision.rs:194-204`). Do not reuse the wrong-endpoint phrases. They mark the request as the wrong API for every vendor (`asks_next`). The honest R-8 behavior for tools and images is: the Cursor member does not take them until R-9, the error does not rest the account, and another member may try.

Attack: 404 for "tools not served" can be read as a missing route. That is still better than a cooled account or a text answer that dropped the tool. R-9 deletes this rejection.

An empty text turn includes the exact substring `an empty reply (the model answered nothing; try again)` (`crates/kernel/codec/src/responses_empty.rs:6`). `rests` is false when the message contains that constant (`decision.rs:20` and `238-246`). The reference's shorter `"an empty reply"` does not contain the constant, so copying it would cool the member. Status is 502, which is in the default failover set, so another member may answer. A stall after real text uses a different sentence (`the upstream stopped sending`) and status 502, so `rests` stays true.

### 4.3 Part 2, R-9, what it adds

Tools. `toolsOf` (`index.mjs:1731-1740`): `tool_choice` none sends no tools; a named choice sends that function; otherwise every function tool. The system blob gains the catalog from `catalog` (lines 1763-1767), which tells the model to call `CallDynamicTool` with namespace `tools`. History encodes assistant tool calls as AI SDK tool-call parts whose tool name is `CallDynamicTool` and whose arguments are `{namespace, toolName, arguments}` (`conversation`, lines 1836-1841`). Toward the caller, replace every `\n` in the exec call id with `__` (`index.mjs:3282`). When writing tool results back into history, `callID` (`index.mjs:1772`) applies only on replay: if the id starts with `call_`, replace `__fc_` with `\nfc_`; otherwise leave the id as stored. Do not describe a general `__` ↔ newline round trip.

A server MCP exec whose tool name is one of the request's tools becomes `ContentBlockStart { block: BlockStart::ToolCall { id, name } }` plus a `ToolArgumentsDelta` holding the full JSON arguments (the wire delivers the arguments in one exec, not as token deltas) and `ContentBlockStop`. Stop reason is tool use. Cursor's own tools stay refused. `GetDynamicTools` (exec field 36) is answered with the request's tools and is not a caller event.

`stall-550`: once any caller call has been emitted, a 60-second wait with only heartbeats, or a checkpoint whose pending set equals the calls already handed over, ends the step with those calls. Calls the server completed by itself count toward the announced total and are not emitted. The step's error text, when the count never arrives and the stream then goes quiet, names how many were listed, emitted, answered locally, and completed by the server (`decode` catch, `index.mjs:3182-3188`).

Reasoning. Interaction field 4 becomes `ThinkingDelta` on its own block. It is not copied into the text block. No signature is invented (`README.md`). Replayed `ContentPart::Thinking` is the reasoning part of the next request's assistant blob.

Images. Encoded as in section 4.1. A base64 `ImageSource` is the hex form. A URL is the placeholder, because the reference does not fetch it.

Usage. On a turn that ends without tool calls, map field 14 through `usageOf` into `hofund_ir::Usage`: `input_tokens` is uncached plus cache read plus cache write, `output_tokens` is field 2, `cache_read_tokens` field 3, `cache_write_tokens` field 4, `reasoning_tokens` field 5, `total_tokens` is input plus output. This is the last `MessageDelta`.

A tool step has no field 14 until the tool results come back on the same Run, and the caller runs tools after this `execute` returns (`index.mjs:1946-1956`). R-9 closes the Run, then `POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetFilteredUsageEvents` (`index.mjs:2076-2079`), not the agent host. Connect JSON headers from `headersFor` (`index.mjs:275-285`, `content-type: application/json`, not Run `application/connect+proto`). Body: `startDate` and `endDate` are decimal strings (`String(since - 60_000)`, `String(Date.now() + 60_000)`, line 2079); `page` and `pageSize` are JSON numbers; optional `teamId` is a JSON number when present (`teamBody`, `index.mjs:177-179`). Window: first wait 1.5 seconds, then every 500 milliseconds, give up at 6 seconds, each request budget 5 seconds (`STEP_WAIT`, line 1973). Claim rows where `conversationId` matches and `timestamp >= since - 1000` (`index.mjs:2088-2100`). Dashboard `tokenUsage.inputTokens` is already the uncached portion; the reference sets `reasoning` to 0 for events (`index.mjs:2102`). Hofund `input_tokens` for the step is `inputTokens + cacheReadTokens + cacheWriteTokens`, not `inputTokens` minus cache again.

If the poll misses, report zeros for that step. Do not estimate. The reference estimates only when the account cannot read events or nothing can collect them later (`finish`, `index.mjs:2977-2982`). An estimate stored in `input_tokens` is what clients read as context fullness. Hofund `Usage` has no deferred-usage bag, and a later `execute` has no ledger to attach it to. The cross-step debt (`owed`, `skipAfter`, the deferred bag) is deferred. Polling once per tool step is the part that fits a stateless call.

Quota on the subscription path stays `GetCurrentPeriodUsage`. R-9 does not move quota onto the Run. It does send `x-cursor-team-id` on that existing call. Step usage is response usage, not the quota window.

Attack on the 6-second poll: it holds the tool-call response for up to 6 seconds after the model has already finished. The reference accepts that so the step is not reported as free. A test with a fake dashboard that answers on the second poll must show the counters, and a fake that never answers must show zeros and must not block past the deadline.

## 5. Tokens and accounts

Three sources, matching `tokenOf` (`index.mjs:368-377`) and `README.md` "Signing in".

| Source | How a token is obtained | When it is renewed | Where the secret lives |
|---|---|---|---|
| Browser PKCE | Existing `sign_in.rs`. Poll `auth/poll`. | It is not renewed on the chat path. `tokenOf` returns the access token until `exp`, then a 401. The reference says the CLI has no renew call for this token. | Account store only. |
| cursor-agent | Keychain on macOS, else `auth.json`. Marker `cursor-agent` (`tokens.rs` `AGENT_MARK`). | When expiry is within 5 minutes (`SOON`, `index.mjs:244`; hofund `REFRESH_MARGIN`), run `cursor-agent status` once and reread with `fresh`. | The account store holds the marker, not the access token. The token is read from the CLI store. |
| API key | `POST /auth/exchange_user_api_key` body `{}`, `keyToken` (`index.mjs:348-364`). Authorization is the key. `Connect-Protocol-Version` is omitted. Network failure and HTTP ≥ 500 map to status 502 (`index.mjs:357-360`). A body without `accessToken` is status 401 (`index.mjs:362`). On the chat path use `PluginError::status_error(502, ...)` and `PluginError::status_error(401, ...)`, not `PluginErrorKind::Transport` or `PluginError::auth`. | Again when the exchanged token's `exp` is within 5 minutes. Cache in memory by SHA-256 of the key. | The account store holds the key. The exchanged access token is memory only. |

Hofund `refresh::oauth_check` posts `/auth/refresh` (`refresh.rs:36-80`). `tokenOf` never does. Keep `refresh()` for an explicit refresh command, serialized by the lease below. The chat path follows `tokenOf`: a browser token past `exp` is a 401, not a silent refresh. A refresh endpoint the reference does not use on chat is how two tasks mint different tokens and the stored one no longer matches the Run.

`CURSOR_API_KEY` is read at import or sign-in, once, and stored. `execute` does not read the process environment. A daemon with two accounts would otherwise bill whichever key happened to be in the environment.

### 5.1 Lease

`AccountsHandle::lease` (`host.rs:168-180`) is the lock. `session::access_token` (`session.rs:11-52`) reads the secret and may refresh with no lease. Two chat calls can both see a near-expiry token and both run `cursor-agent status` or both exchange the API key.

Every renew on the chat path and on `Subscription::refresh` takes `accounts().lease(id, 30s)` first, rereads the secret, and renews only if the reread is still near expiry. The in-memory API-key cache update happens inside that lease. A lease timeout is `Io` and fails this call. It does not start a second renew. `cliRefresh` in the reference is a single in-flight status command (`index.mjs:255-262`). The lease is the cross-task version. An in-process mutex is not enough: the comment on `lease` says other processes wait too.

### 5.2 Issue 609

Keychain (`cli.rs:21-102` versus `index.mjs:182-216` and `cursor.go:234-265`).

- Reuse a successful read for 10 seconds (`TOKEN_TTL`, `cursorTokenTTL`).
- Exit status 44 means the item is absent (`keychainDenied`, `account.go:546-551`). Clear the denial timestamp and fall through to `auth.json`.
- Any other non-zero exit is a denial. Do not spawn `security` again for 10 minutes (`KEYCHAIN_DENIED`, `keychainDeniedTTL` in `account.go:407-411`), unless the caller passes `fresh` (sign-in and the reread after `status`).
- `fresh` bypasses both the 10-second cache and the denial period.
- Overlapping callers share one in-flight spawn (`keychain.busy`).
- The read has a 10-second timeout on the JavaScript `run` helper (`index.mjs:63-66`). On timeout, `run` reports exit code `-1`, which `keychainToken` treats like any non-44 failure and sets the 10-minute denial (`index.mjs:206-209`). The Go provider's `security` call has no timeout; `cursor.go:260-261` is the `keychainDenied` branch (exit ≠ 44), not a timeout path. Hofund intentionally does **not** set the 10-minute denial on timeout: a stuck keychain dialog must not block the daemon for ten minutes or masquerade as the user pressing cancel. Drop the child after the timeout, leave `ask` unset, and allow the next read after the 10-second success TTL expires with an empty token. Document this deviation against the JavaScript contract in the adopt-or-redesign table; Go remains the sign-in and exit-44 contract only.
- This needs `PipedChild::exit_code` from section 3. Stdout alone cannot tell 44 from a denial. Today's code drops the child after reading stdout (`cli.rs:98`), which kills it and loses the status. Wait for exit after stdout EOF.

`list_models` (`lib.rs:326-363`, `account_label` at 400-406) writes the access token to `{state}/cursor-models/{sanitized account id}/auth.json` and never removes it. The directory name is derived from the account id, which can carry the mailbox.

Replace that with a directory under the process temp dir, mode `0o700`, created for the call and removed on drop, including the error path. The directory name is the hex SHA-256 of the account id. `cursor-agent` still needs `CURSOR_CONFIG_DIR` pointed at that directory and an `auth.json` for the duration of `models` (file mode owner-read/write as `auth::write_auth_file` already sets). A crash can leave a temp directory. The name is not the mailbox, and it is not under the state dir that lives as long as the install. Do not add the `tempfile` crate to the plugin. `std::fs` is already used by `auth.rs`.

`Endpoints` is re-exported and its fields are public (`lib.rs:30`, `endpoints.rs:4-11`). `CursorFactory::test_agent_home` and `skip_keychain` are public (`lib.rs:64-68`). `factory_with_endpoints` is public (line 51). Tests live in the same crate. Make the fields `pub(crate)`, stop the public re-export, and mark `factory_with_endpoints` `#[doc(hidden)]` and `pub(crate)`. A test hook that is `pub` is part of the crate's API surface for every downstream user.

### 5.4 Team id from cli-config.json

One in-process cache of `cli-config.json` (`teamRead`, `TEAM_TTL = 10_000`, `index.mjs:152-166`). It is not keyed by access-token hash. Re-read the file at most every 10 seconds; a parse failure keeps the previous `authInfo`; only `ENOENT` clears it.

Send decimal `x-cursor-team-id` only when `authInfo.authId` is non-empty, equals the JWT `sub` (`tokenSub`, `index.mjs:131-139`), and `activeTeamId` is a safe integer greater than zero (`teamOf`, line 142). Omit the header otherwise (`team-id.test.mjs`).

`GetCurrentPeriodUsage` and `GetHardLimit` use body `{}`; the team rides in the header only (`index.mjs:171-172`, `team-id.test.mjs`). `GetFilteredUsageEvents` adds `teamId` as a JSON number in the body when a team exists (`teamBody`, `index.mjs:177-179`).

R-8 applies this on Run headers, agent config fetch, and the existing quota client. R-9 adds the header on tool-step dashboard polls and any new dashboard calls.

### 5.5 What is never written

The account store is the only durable copy of a browser token or an API key. Chat does not write `auth.json` except for the `list_models` temp file above. The agent path does not copy the keychain token into the account secret. Logs and `Debug` keep using the existing secret redaction. A wire-trace option that records frames is deferred. The reference's trace redacts tokens (`native-session.mjs` `WireTrace`); a first version that logs a frame for debugging will log a bearer token. Do not add it in R-8 or R-9.

## 6. Error classification

`failure` / `statusOf` (`index.mjs:1650-1686`) and `errors.test.mjs` are the word list. Hofund must not copy the reference's "sign in again in {product}" suffix. The message is the server's text plus a short hofund clause, "sign in to Cursor again", with no other product name.

Classification order matches `statusOf` (`index.mjs:1675-1686`): `regional(msg)`; then Connect code `permission_denied` (403); then `unauthenticated`, HTTP 401, or message containing `expired`; then `resource_exhausted` or quota, usage-limit, or rate-limit wording (the reference maps these to one 429 — Hofund splits quota from rate); then too-long wording; then other Connect code `invalid_argument`; then `unavailable`; then any other non-empty Connect code (502); finally, when `failure` has no Connect code, keep the HTTP status (`failure` → `statusOf(status, "", msg)`, `index.mjs:1655`, `1686`; `errors.test.mjs` `failure(402, "")`, `failure(429, "")`). A regional message stays 403 even when the Connect status was 401.

Map to kinds the router already understands. Do not add a `PluginErrorKind`. Do not add Cursor phrases to `crates/kernel/router`. `failure_kind` (`cooldown.rs:1453-1488`) and `allows` (`decision.rs:171-204`) are the rest rules.

These rows follow that order for Connect failures. R-8 implements the rows a text-only Run can hit; R-9 adds Max Mode retry without changing the mappings.

| Reference outcome | HTTP | Hofund error | Rest |
|---|---|---|---|
| Message contains `region`, after the one host retry | 403 | `status_error(403, server text)`. The reference forces 403 even when the Connect status was something else (`statusOf`, lines 1675-1676). | Generic ladder, because 403 is generic, not quota. Failover yes. `REGION_WORDS` will not special-case it. That is acceptable: the plugin already retried the regional host. |
| Connect code `permission_denied`, when not regional | 403 | `status_error(403, server text)` (`index.mjs:1676`) | Generic ladder. Failover yes. |
| Connect code `unauthenticated`, HTTP 401, or message containing `expired` (`errors.test.mjs` "any expired is the built-in's 401", including a trial), when not regional | 401 | `status_error(401, ...)`. Not `PluginError::auth`. `Auth` does not fail over. | Generic ladder (1 minute, doubling, cap 10 minutes). 401 is in the default failover set, so the next account is tried. |
| Connect code `resource_exhausted`, or text containing `quota` or `usage limit` | 429 | `quota_status(429, "usage limit reached: ...")` (`index.mjs:1679-1680`) | Exhausted-quota rest, 15 minutes or a vendor hint, and failover. |
| Text containing `rate limit` without quota or usage-limit wording | 429 | `status_error(429, ...)` | Rate ladder (1 minute, doubling, cap 30 minutes), and failover. |
| `too long`, `context length`, `too many tokens`, including Connect `invalid_argument` whose text says the prompt is too long | 400 | `status_error(400, "input is too long for the model's context: ...")`. That phrase matches `too_long` (`crates/kernel/ir/src/error.rs:12-14`) (`index.mjs:1681-1682`). | `rests` is false. `allows` returns false when `overflowed` (`decision.rs:184-186`), so this error does not fail over by status list. The router tries the next member from the earlier `outgrown` loop when every account of the model is too small (`router/src/lib.rs:1908-1924`). Keep the too-long sentence in tests. |
| Other Connect code `invalid_argument` | 400 | `status_error(400, server text)`, `without_cooldown` when the text does not match quota or unserved-model words (`index.mjs:1683`) | 400 does not cool (`failure_kind` has no arm for a bare 400). It also does not fail over unless the text hits an existing list. Leave the server text intact. |
| Connect code `unavailable` | 503 | `status_error(503, server text)` (`index.mjs:1684`) | Generic ladder, unless the message contains `overloaded`, in which case `vendor_overloaded` (`ir/src/error.rs:80-86`) makes it a 30-second overload rest. Failover yes. |
| Any other Connect code with a non-empty `code` | 502 | `status_error(502, server text)` (`index.mjs:1685`) | Generic ladder. Failover yes. |
| HTTP failure with no Connect code (empty body or non-JSON) | unchanged | `status_error` with the same HTTP status and `statusText` (`index.mjs:1655`, `1686`; `errors.test.mjs` lines 11-12) | Follow `failure_kind` for that status. Empty `failure(429, "")` stays 429, not the quota split. |
| Model not allowed, when the text matches `UNSERVED_WORDS` and is not quota text, status 400, 403, 404, or 422 | as mapped | preserve the server text | `model_unserved` uses the generic ladder on that model only (`cooldown.rs:1460-1461`). Do not rewrite the text to force a match. If a captured phrase does not match, add a vendor-neutral pattern to `UNSERVED_WORDS` in a router change. Do not put "Cursor" in the router. |
| Max Mode required | — | R-9: one retry inside `execute` with model details field 7 and requested field 2 set to 1, then `maxOnly` remembers the id (`maxRequired`, `index.mjs:3328`, `3392-3399`) | The router does not see the first failure. |
| Empty text turn | 502 | message contains the empty-reply constant from section 4.2 | Does not rest. Does fail over. |
| Stall (idle or quiet) before any text, before `Ok(stream)` | 502 | idle/quiet diagnostic (not the empty-reply constant, not `the upstream stopped sending`) | Generic 502 ladder (`rests` true). Failover yes, because no stream byte was returned. |
| Stall after output started | 502 | `the upstream stopped sending`, no empty-reply constant | Rests. Does not fail over, because bytes already went out. |
| Caller cancel | — | `Cancelled` | Does not rest. |
| HTTP/2 open failed and HTTP/1.1 then succeeded | — | not returned | Does not rest. |
| Both transports failed before any byte | 502 | `status_error(502, ...)` or the connect error if no HTTP request was sent | Connect-class errors follow `failover.connect_errors`. A 502 rests and failovers. |
| `BidiAppend` deadline | 504-class in the reference's end frame, code `deadline_exceeded`, which `statusOf` turns into 502 because the code is neither quota nor auth | `status_error(502, ...)` | Generic ladder. |

`PluginError::auth` stays on the subscription methods (`refresh`, sign-in, a missing stored secret). Those methods are not the router. Using it for a Run would pin the request to a dead account.

Attack: a 403 region rest cools a healthy account for a minute because the user is in the wrong region. The retry already happened. A one-minute generic rest is the same rest any 403 gets. Special-casing it in the router is a vendor rule in a crate that forbids vendor names. Leave it.

Attack: treating every `expired` as 401 marks a trial expiry as a sign-in failure. `errors.test.mjs` requires that, and the account is kept (the reference sets a sign-in header to `kept` and does not drop the account). Hofund must not delete the account on this error.

## 7. Testing

No test dials a public host. The reference enforces that with `nonet.mjs`. Hofund enforces it by construction: the only sockets are loopback listeners started by the test kit.

The plugin cannot bind a socket. `TcpListener` is a disallowed type (`xtask/plugin-boundary/clippy.toml`). Unit tests under `plugins/subscriptions/cursor/src` are plugin code. The fake servers live in `hofund-kit-subscription`, which is already a dev-dependency, and they bind `127.0.0.1` only. The kit does not mention Cursor. It exposes:

- an HTTP/1.1 server that records requests and returns a scripted body or a chunked body
- an HTTP/2 server on cleartext prior knowledge that accepts one bidirectional stream, records frames, and writes scripted frames

The Cursor tests pass Connect bytes to those servers. A separate plugin-host test uses a local `https` certificate and `HttpProfile::insecure` to prove the ALPN client, not prior knowledge, completes a duplex write after headers. That test is generic. It does not speak Connect.

Golden frames, in the plugin, with no socket:

- `AgentRunRequest` action nesting: field 2 length-delimited, inner field 2 length-delimited, innermost request context (`index.mjs:2282-2283`)
- text delta, thinking delta, heartbeat, turn-end usage, tool count
- exec MCP arguments and the refusal frame
- KV get: golden replies for blob found (client field 3 → field 2 → field 1 = bytes) and not found (client field 3 → field 2 → field 2 → field 1 = `"blob not found"`), plus store ack (inner field 3 empty), `index.mjs:3161-3167`
- exec field 10 environment reply and field 36 connected-server reply, each followed by `closeExec`
- `BidiAppend` body field numbers and sequence 0 then 1
- gzip flag bit, inflated payload
- truncated frame, leftover byte, 64 MiB plus one rejected
- a varint above 2^53
- outbound Run bytes do not contain `hofund` or the reference product name (`upstream-words.test.mjs`)

Session tests against the kit servers:

- HTTP/2 happy path: headers, then the client frame, then a text frame, then a turn end. Assert the request path is `/agent.v1.AgentService/Run` and the headers in section 1.2, including team id when `cli-config.json` matches the token subject (`team-id.test.mjs`).
- Head delayed past 15 seconds: the plugin opens RunSSE and `BidiAppend` on HTTP/1.1, and the next Run does not open HTTP/2 until the memory is cleared or 10 minutes are injected on the test clock.
- Connection refused: this Run falls back, the next Run tries HTTP/2.
- `REFUSED_STREAM` before headers: this Run falls back, the next Run tries HTTP/2.
- Region body on HTTP/1.1: one HTTP/2 retry at the refreshed agent URL.
- Sixteen blob replies in flight, and a following non-KV frame that does not start until those sixteen complete. A 17th KV reply waits.
- Append timeout ends the Run.
- Idle 60 seconds with a shrunk clock, and 240 seconds of heartbeats only, both become the stall error and do not set the HTTP/2 memory.
- R-9: the `stall-550.test.mjs` trace shape (two server-completed calls, one exec, a count, a checkpoint, then heartbeats) emits one caller tool call and then ends, without waiting 240 seconds.
- Cancel drops the duplex session and does not append further.

Conformance: `plugins/subscriptions/cursor/src/conformance.rs` currently expects `upstream()` to be unsupported. Replace that assertion with the kit's subscription conformance once `execute` returns a stream. Keep the existing sign-in, import, quota, and model-list tests. Add the lease test: two overlapping near-expiry refreshes spawn `cursor-agent status` once. Add the keychain tests: exit 44, exit non-44, timeout, `fresh` bypass, single flight. Add the `list_models` test: the state dir has no `cursor-models` child after success and after a failing CLI, and the temp directory name is a hash.

Mutation-sized assertions, the ones a one-character edit should kill:

- team id omitted when `authInfo.authId` is not the JWT `sub` (`team-id.test.mjs`)
- team id omitted when `activeTeamId` is 0 or not an integer (`teamOf`, `index.mjs:142`)
- `GetCurrentPeriodUsage` body stays `{}` when the team id is sent (`team-id.test.mjs` first test)
- cache read and cache write are subtracted from input before being added back into `input_tokens`
- thinking bytes are absent from the text block
- a tool result is not submitted to a refused exec
- HTTP/2 memory is not set by `stream refused`
- empty-reply wording contains the codec constant, and the stall-after-text wording does not
- API-key exchange does not write `accessToken` into the account secret
- `list_models` does not leave `auth.json` behind

## 8. Work breakdown

### R-8, part 1

Goal: one account can stream text. Tools, images, reasoning events, and usage counters are absent on purpose.

| Piece | Files | Size | Risk |
|---|---|---|---|
| Duplex API and HTTP/1.1-only pool | `crates/kernel/plugin-api/src/host.rs`, `kernel.rs`; `crates/kernel/plugin-host/src/pool.rs`, `Cargo.toml`; SDK future helper | about 700 lines plus tests | Enabling `reqwest` `http2` without pinning the unary client to HTTP/1.1 changes every plugin. The unary test that a cleartext request sends `HTTP/1.1` is the guard. |
| Gzip helper | `crates/kernel/plugin-sdk` | about 80 lines | `cargo deny` on `flate2` / `miniz_oxide`. No C backend. |
| Piped exit status | `plugin-api` `PipedChild`, `plugin-host` `piped.rs` | about 120 lines | A drop-kills-before-wait race hides exit 44. |
| Issue 609 | `cursor/src/cli.rs`, `lib.rs`, `endpoints.rs` | about 250 lines | Denial TTL that treats timeout as denial will hide a slow keychain for 10 minutes. Timeout must not set it. |
| API-key exchange and lease | `tokens.rs`, `session.rs`, `refresh.rs`, new `exchange.rs`, `import.rs` | about 350 lines | Writing the exchanged token into the account store creates a second source of truth. Memory only. |
| Team header | `connect.rs`, agent-host module | about 200 lines | Wrong team billing when `authId !== sub`. One process-wide `cli-config.json` cache (`TEAM_TTL`, section 5.4), not keyed by token hash. |
| Codec and Run session | new `proto.rs`, `frame.rs`, `run.rs`, `upstream.rs` | about 1,400 lines | The stateless Run still has to answer KV and refuse exec, or the first real server will stall. |
| Mapping and stream | `upstream.rs` | about 500 lines | Retrying region after the first delta duplicates billing. Retry only before `Ok`. |
| Run error classification | `failure.rs` | about 300 lines | Maps Connect and HTTP/1.1 failures to the section 6 table for 401, 403, 429, 502, overflow, empty reply, and stall. Tests assert kind, status, and wording without linking the router crate. |
| Tests and docs | `src/tests.rs`, `conformance.rs`, kit fakes, `docs/coverage/subscription-parity.md`, the parity matrix, the adopt-or-redesign table required by `AGENTS.md` | about 1,200 lines | A fake that accepts any path will not catch a wrong RPC name. Assert the path. |

Architecture crate edges do not change. `expected_deps` stays. External crates change: `reqwest` features, `flate2` on the SDK. Both go through `cargo deny`.

### R-9, part 2

Depends on R-8's Run.

| Piece | Files | Size | Risk |
|---|---|---|---|
| Tool catalog, history, exec mapping, stall-550 | `run.rs`, `upstream.rs` | about 800 lines | Emitting a server-completed call as a caller tool makes the client run a tool the server already finished. |
| Reasoning blocks | `upstream.rs` | about 150 lines | Concatenating thinking into text changes the next turn's roles. |
| Tool-step dashboard usage | `run.rs`, `connect.rs` | about 200 lines | The poll holds the tool response. The deadline test is mandatory. Zeros on a miss, never an estimate. |
| Effort parameters (picker level lists) | `upstream.rs` | about 150 lines | Defer with the model picker; do not invent levels in the plugin. `fast=true` by default over-bills. Leave fast off. |
| Max Mode: `maxRequired` retry and `maxOnly` memory | `upstream.rs` | about 100 lines | One unconditional retry when the error matches and the first attempt was not Max Mode (`index.mjs:3392-3399`). Independent of the picker. |
| Images | `upstream.rs` | about 150 lines | Fetching a remote image URL is not what the reference does. Placeholder only. |

### Explicitly deferred

- Native retention: the owner pool, 15-minute lifetime, 120-second result wait, 10-minute continue reserve, continuity header, replay of an identical retry, and `native-session.mjs` as a whole. A restarted gateway already takes the stateless path. Retention is a later item.
- Cross-step usage debt, the deferred-usage bag, and the "stop waiting after three misses" account flag. There is no IR field and no ledger that survives `execute`.
- Stable `conversation_id` and prompt-cache affinity. Needs a caller-supplied key on `Request`.
- The model picker: `AvailableModels`, `GetUsableModels`, catalog scope 3, `@size` ids, fast variants, the 10-minute list shared by in-flight requests (`listFor`, `index.mjs:1324-1344`), and per-model effort level lists for `fitEffort`. R-8 sends the requested id. `auto` is sent as `auto`. Max Mode retry and `maxOnly` are R-9 plugin behavior, not picker output.
- Historical MCP namespace alias on read (`NAMESPACES` / `bareName`, `index.mjs:1749-1750`). Outbound stays `tools` only until a branding-safe approach exists.
- `service_tier` / `fast`.
- Wire trace.
- Web search and every Cursor-owned tool. They stay refused.
- Enterprise `usage-summary` and included-only routing. Quota already has a separate path.
- Free credit beyond included usage: plan bonus (`GetCurrentPeriodUsage` `planUsage.bonusSpend`, `remainingBonus`, `index.mjs:544-599`) and active grants (`GetUsageLimitStatusAndActiveGrants` `activeGrants`, cents strings, model allow-list, client type `cli`, `index.mjs:544-578`), plus the rule that usable credit keeps the account serving after included usage is spent. R-9 quota routing can adopt both windows or defer them together in one item.
- Streaming tool-argument deltas. The exec carries the full arguments.
- A second retry after the client has seen a delta.

### Order

1. Kernel duplex, HTTP/1.1 pin, piped exit status, SDK inflate and future helper. Land this before the plugin uses it, with generic tests.
2. Issue 609 and the API-key exchange, still with `upstream()` unsupported. These are account bugs, and they are independently testable.
3. R-8 Run, text stream, and Run error classification (section 6 rows for 401, 403, 429, 502, overflow, empty reply, stall).
4. R-9 tools, reasoning, tool-step usage poll, Max Mode retry, effort when the picker exists.

Each step updates the parity rows it touches. Stage 2 auxiliary work stays behind stage 1, as `AGENTS.md` requires. R-8 and R-9 are that stage's Cursor upstream, not a reason to start auxiliary features.
