# Control plane and hot reload

This is the plan for row L of RFC 0001. It covers the management protocol, its clients and transports, the `hofundd` process boundary, live configuration reload, and the event stream. It is a design document only. No behavior or parity status changes with this document.

RFC 0001 assigns L the control crates, `/v1/hofund/*`, token-authenticated loopback and local IPC, an event stream, `apps/hofundd`, the registry-link move, `hofund serve` exec, and snapshot reload (`docs/rfc/0001-microkernel-plugins.md:695-706,776-784,843-857`). The review resolutions require `hofundd` to be the only direct registry linker and the daemon library to receive the registry as an argument (`docs/rfc/0001-microkernel-plugins.md:1097,1112-1116`). The implementation must also close the control-plane parts of feature-parity rows 11.2 and 12.3 (`docs/feature-parity.md:277,288`).

Reference citations in this plan are against yetone/magpie `dc1cfe015675e8c3529cb67b6de3c40991ef2c13`, which is newer than the RFC pin `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`. Between the two pins, these cited files changed: `internal/cli/providers_cli.go`, `internal/gateway/busy.go`, `internal/gateway/gateway.go`, `internal/gateway/oneport.go`, `internal/gui/api.go`, `internal/gui/backend.go`, `internal/gui/web.go`, `internal/middleware/middleware.go`, and `internal/settings/settings.go`. `internal/filememo/watch.go` does not exist at the RFC pin. Every other cited file is unchanged between the pins.

## Scope and non-goals

L delivers one long-running daemon that serves the model gateway and a separate management listener. The CLI uses that management API for runtime operations. `hofund serve` starts `hofundd`. Configuration changes validate and publish without interrupting in-flight model calls. Clients receive lifecycle and request-summary events.

L includes the stable DTO crate, transport client, loopback TCP, Unix domain socket, Windows named pipe, token and same-user checks, health and status, configuration editing and reload, provider and model operations, agent operations, usage, token rotation, events, and graceful shutdown. It includes the cooldown lifetime fix from issue #117 and release packaging for two CLI binaries.

The following wait for later pull requests:

- Desktop removal of its embedded daemon. RFC 0001 explicitly makes this a follower of L (`docs/rfc/0001-microkernel-plugins.md:703-706`).
- A TUI and a browser UI. They can later use the same client, but L adds neither application.
- Socket handoff on update, feature-parity row 16.3. A management connection is not a handed-off model listener. Handoff needs listener ownership transfer, update orchestration, and drain behavior of its own (`docs/feature-parity.md:329`).
- Background-service installation, row 16.4, including systemd and launchd integration (`docs/feature-parity.md:330`).
- Full routing-history, quota-history, request-body capture, profiling, named gateway keys, and service-management APIs. Request summaries and cooldown events are the L subset, not those later surfaces.
- The `apps/cli` mechanical move. It stays in row J, after L and I, as RFC 0001 already orders it (`docs/rfc/0001-microkernel-plugins.md:745,856-857`). Moving it in L would mix process changes with a tree-wide mechanical move.

## Today

### CLI and desktop paths

`hofund` currently links `hofund-daemon` and `hofund-agent` directly (`crates/cli/Cargo.toml:18-20`). Its top-level dispatch calls `serve_blocking`, `check`, token and usage functions, or local agent adapters (`crates/cli/src/main.rs:15-50`). The command parser has no provider, model, status, healthcheck, or shutdown command (`crates/cli/src/main.rs:120-145`).

| Command | Current path |
| --- | --- |
| `hofund serve` | `serve_blocking` builds a Tokio runtime and calls `serve` in the same process (`crates/cli/src/main.rs:24-30`; `crates/daemon/src/lib.rs:242-260`). |
| `hofund config check` | `check` loads the file and builds every provider without binding a listener (`crates/cli/src/main.rs:343-351`; `crates/daemon/src/lib.rs:67-76`). |
| `hofund token show` | `token_show` resolves the configured token path and calls `TokenFile::load_or_create` (`crates/daemon/src/lib.rs:79-95,263-266`). |
| `hofund token rotate` | The CLI reads the local gateway and route models, rotates the file, then refreshes every connected agent and reports failures (`crates/cli/src/main.rs:353-393`). |
| `hofund usage` | `usage_report` opens the configured SQLite ledger and renders its table (`crates/cli/src/main.rs:37`; `crates/daemon/src/lib.rs:157-166`). |
| `hofund agent list` | The CLI builds a catalog from `agent_descriptions`, which the daemon obtains from the registry, and probes installation through local agent sessions (`crates/cli/src/main.rs:309-341`; `crates/daemon/src/lib.rs:141-145`). |
| `hofund agent connect` | The CLI gets the gateway URL, token, and route models from the daemon, then calls the adapter's `plan` or `apply` locally (`crates/cli/src/main.rs:395-440`). |
| `hofund agent disconnect` | The CLI calls the local adapter's `disconnect` (`crates/cli/src/main.rs:472-486`). |
| `hofund agent status` | The CLI builds the same target and calls each local adapter's `status` (`crates/cli/src/main.rs:488-501`). |
| Help and version | These are local text and package-version operations (`crates/cli/src/main.rs:24-33,120-127`). |

The desktop has the same embedded composition root. `Control` stores an `Option<Running>` and calls `hofund_daemon::start` and `Running::shutdown` for Start and Stop (`apps/desktop/src-tauri/src/control.rs:143-190`). Agent changes and status call local adapters with daemon-derived targets, usage calls `usage_report`, and snapshots read the configured listener (`apps/desktop/src-tauri/src/control.rs:192-225,227-291`). Tauri commands and menu dispatch invoke those methods directly (`apps/desktop/src-tauri/src/lib.rs:135-175,187-251`). Open Config creates the file through `ensure_config_file` and launches the platform opener (`apps/desktop/src-tauri/src/open.rs:1-43`). The desktop manifest therefore links both `hofund-daemon` and `hofund-agent` (`apps/desktop/src-tauri/Cargo.toml:16-21`).

### Listener, token, router, and models

The default model listener is `127.0.0.1:3425`; the token file defaults to `hofund.token` beside the configuration file (`crates/config/src/lib.rs:805-815`; `crates/daemon/src/lib.rs:789-798`). Startup loads the configuration, creates or reads the token, opens usage and model state, builds the router and gateway, binds the configured `SocketAddr`, and starts model refresh (`crates/daemon/src/lib.rs:204-235`). `Gateway::bind` uses a Tokio `TcpListener` and `axum::serve` with graceful shutdown (`crates/gateway/src/lib.rs:176-203`).

The gateway's `/health` route is unauthenticated. Other model and inbound-surface routes require authentication (`crates/gateway/src/lib.rs:143-174,240-256,350-356`). Authentication accepts Bearer, `x-api-key`, `x-goog-api-key`, or a query key and compares each presented value in constant time (`crates/gateway/src/lib.rs:525-538,557-607`). With a token file, every request re-reads it; an unreadable or empty file leaves the last good token in force (`crates/gateway/src/lib.rs:96-109,541-555`). Rotation writes and syncs a temporary mode-`0600` file on Unix and renames it over the old file (`crates/secrets/src/lib.rs:102-113,126-169`).

`router()` currently creates a new `Arc<Cooldowns>` on every invocation, builds providers with that book, and passes it to membership (`crates/daemon/src/lib.rs:268-314`). Membership stores the book in its table and injects the same `Arc` into every router it publishes (`crates/daemon/src/membership.rs:45-50,146-160,205-226`). The `Snapshot` wraps `ArcSwap<Router>`; each request keeps the `Arc<Router>` it loaded, so a store does not alter that request's router (`crates/router/src/snapshot.rs:14-20,46-58`). There is no configuration reload today; the architecture document reserves it for L and distinguishes token-file re-reads from configuration reload (`docs/architecture.md:234-243`).

`ModelsHost` already owns configured-instance registration, persistent model state, list aggregation, and probes (`crates/daemon/src/models.rs:166-220`). Its `list_models` and `probe` methods are async host operations, and probe refreshes the list before selection (`crates/daemon/src/models.rs:433-517`). L exposes these operations through control calls; it does not put listing or probing in the client.

## Crate layout and dependency edges

Add these packages when their code pull requests land:

| Path | Package | Responsibility |
| --- | --- | --- |
| `crates/control-proto` | `hofund-control-proto` | Versioned serde DTOs, event types, error envelope, and wire-version constant. No HTTP, Tokio, daemon, router, provider, or plugin-api dependency. |
| `crates/control-client` | `hofund-control-client` | Discovery, authentication, request and SSE decoding, reconnect, and typed methods over each transport. Re-exports the proto for UI callers. |
| `apps/hofundd` | `hofund-daemon-bin`, binary `hofundd` | Process composition and the only direct `hofund-plugin-registry` link. Constructs the registry adapter and passes it to `hofund-daemon`. |

`hofund-daemon` defines a small `DaemonRegistry` interface in terms of plugin-api factory and description types. `apps/hofundd` implements it by calling the registry. The library never names `hofund-plugin-registry`. Until the switch pull request, the existing in-process CLI and desktop receive an adapter from a temporary compatibility constructor; after the switch, only `hofundd` constructs the production adapter. Daemon tests use a fake adapter rather than adding a registry dev-dependency.

The gateway crate owns axum and hyper serving because it already owns the HTTP edge (`crates/gateway/src/lib.rs:19-37,176-196`). The daemon supplies a control-state interface and DTOs; the gateway maps HTTP to that interface. `control-client` uses hyper and hyper-util for TCP and local IPC, not reqwest, because one connection implementation covers all three transports. The architecture's HTTP rule must therefore continue to allow axum only in gateway while explicitly allowing hyper, http, http-body-util, and hyper-util in gateway and control-client. These crates are already locked: axum 0.8.9, hyper 1.12.0, http-body-util 0.1.5, hyper-util 0.1.21, Tokio 1.54.1, and serde 1.0.229 (`Cargo.lock:137-140,1892-1895,1912-1915,1937-1940,1975-1978,3720-3723,4551-4554`). No SSE, WebSocket, or file-watcher dependency is needed.

### Exact architecture-test changes in the code pull requests

The design PR changes only the inspiration allowlist. The implementation pull requests make the following changes to `MEMBERS` and `expected_deps` in `xtask/tests/workspace_invariants.rs` and to the normative table and diagram in `docs/architecture.md`.

1. Add `apps/hofundd` immediately after `apps/desktop/src-tauri` in `MEMBERS`. Add `crates/control-client` and `crates/control-proto` immediately after `crates/config`. Add the same members to the root workspace list and add sorted workspace dependency entries for `hofund-control-client` and `hofund-control-proto`.
2. Add `"hofund-control-client" => &["hofund-control-proto"]` and `"hofund-control-proto" => &[]`.
3. Add `"hofund-daemon-bin" => &["hofund-daemon", "hofund-plugin-host", "hofund-plugin-registry"]`.
4. Add `hofund-control-proto` to the existing gateway and daemon lists. Remove `hofund-plugin-registry` from the daemon list in the registry-switch pull request. Do not add a registry edge to either control crate.
5. At the CLI switch, set `"hofund-cli" => &["hofund-config", "hofund-control-client", "hofund-secrets"]`. Config supports explicit offline structural checks, and secrets supports local `token show`; every full validation and runtime operation uses the client. Remove the daemon and agent edges.
6. In the later desktop rework, not in L, set `"hofund-desktop" => &["hofund-control-client"]`. The client re-export supplies DTOs, so desktop needs no direct proto edge.
7. In row J, replace the member path `crates/cli` with `apps/cli` and move it beside the other apps. Do not combine that move with L.

`docs/architecture.md` must gain the same nodes, edges, and crate responsibilities in the pull request that introduces them. Its composition-root text must name `hofundd`, and its direct-registry rule must name only `hofund-daemon-bin` after L. The current design PR leaves that document unchanged because none of those edges exists yet.

## Protocol

### Versioning and DTO rules

Every management path starts `/v1/hofund`. JSON request and response bodies carry `"version": 1`; query-only and empty requests use the path version. `hofund-control-proto` exports `API_VERSION: u32 = 1`. A mismatched version is an explicit protocol error, not a best-effort decode.

Public DTOs use serde and `#[non_exhaustive]`. Requests reject unknown fields so a typo cannot change meaning. Responses permit unknown fields so an older client can read a newer compatible server. Optional response fields are additive. Enum consumers match a wildcard; response enums that serde must decode include an `Unknown` fallback.

```rust
pub const API_VERSION: u32 = 1;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ConfigApplyRequest {
    pub version: u32,
    pub expected_revision: String,
    pub text: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ConfigResponse {
    pub version: u32,
    pub revision: String,
    pub generation: u64,
    pub text: String,
    pub restart_required: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ErrorEnvelope {
    pub version: u32,
    pub error: ControlError,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ControlError {
    pub kind: ControlErrorKind,
    pub message: String,
    pub request_id: String,
    pub retry_after_ms: Option<u64>,
    pub details: Vec<FieldIssue>,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorKind {
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    InvalidConfig,
    Unavailable,
    Upstream,
    Timeout,
    Internal,
    #[serde(other)]
    Unknown,
}
```

Times on the wire are Unix milliseconds. Durations such as retry hints are unsigned milliseconds. A cooldown's monotonic `Instant` is converted to a remaining duration or wall-clock deadline by the server; neither `Instant` nor `Duration` appears in the proto. Text limits are explicit: configuration bodies at most 1 MiB, error messages at most 4 KiB, and request IDs bounded ASCII.

The proto crate starts at 0.1.0. Before 1.0, additive DTO or event changes are patch releases and breaking changes require a minor release plus a new `/vN` route. After 1.0, those categories become minor and major respectively. The server and client check both path and body versions. CI keeps a public-API snapshot and committed JSON snapshots for every DTO and event.

Issue #121 concerns the public plugin and cooldown Rust types. The control protocol does not re-export or serialize `ProviderErrorKind`, `ProviderError`, `CooldownPolicy`, `CooldownCause`, or plugin-api types. It maps failures to `ControlErrorKind`, optional HTTP status, retry milliseconds, and a sanitized message. Cooldown status uses a separate non-exhaustive wire enum and stable member id string. Issue #121 still needs its own Rust API decision; L does not resolve it by leaking those types into another public crate.

### Routes

All routes except `GET /health` require authentication. DTO names below are sketches for the proto, not re-exports of current internal types. `EmptyRequest` means no body; path and query parameters remain validated.

| Method and path | Request DTO | Response DTO | Initial caller |
| --- | --- | --- | --- |
| `GET /v1/hofund/health` | `EmptyRequest` | `HealthResponse` with API and daemon versions | CLI `healthcheck`, desktop reachability |
| `GET /v1/hofund/status` | `EmptyRequest` | `StatusResponse` with PID, start time, listener labels, generation, revision, in-flight count, shutdown state | CLI `status`, desktop |
| `GET /v1/hofund/config` | `EmptyRequest` | `ConfigResponse` | CLI and desktop editor |
| `POST /v1/hofund/config/validate` | `ConfigValidateRequest` | `ConfigValidationResponse` with issues and restart-required fields | CLI `config check`, desktop save preview |
| `POST /v1/hofund/config/apply` | `ConfigApplyRequest` | `ConfigResponse` | Desktop save; CLI explicit apply |
| `POST /v1/hofund/config/reload` | `ReloadRequest` with expected revision | `ConfigResponse` | CLI `config reload`, SIGHUP uses the same internal operation |
| `GET /v1/hofund/providers` | `EmptyRequest` | `ProviderListResponse` | CLI and desktop Providers page |
| `POST /v1/hofund/providers/{id}/test` | `ProviderTestRequest` with optional model ids | `ProviderTestResponse` | CLI `provider test`, preserving the #28 operation through L |
| `GET /v1/hofund/providers/{id}/models` | `EmptyRequest` | `ModelListResponse` | CLI `provider models`, desktop |
| `POST /v1/hofund/providers/{id}/models/refresh` | `EmptyRequest` | `ModelListResponse` | CLI `provider refresh`, desktop refresh |
| `GET /v1/hofund/models` | `EmptyRequest` | `RouteModelListResponse` | Agent plan and CLI route listing |
| `GET /v1/hofund/agents` | `EmptyRequest` | `AgentListResponse` | CLI `agent list`, desktop |
| `GET /v1/hofund/agents/status` | `EmptyRequest` | `AgentStatusResponse` | CLI and desktop agent status |
| `POST /v1/hofund/agents/{id}/plan` | `AgentPlanRequest` with connect or disconnect and force flag | `AgentPlanResponse` with redacted changes and plan revision | CLI dry-run and desktop confirmation |
| `POST /v1/hofund/agents/{id}/connect` | `AgentConnectRequest` with optional expected plan revision | `AgentChangeResponse` | CLI and desktop connect |
| `POST /v1/hofund/agents/{id}/disconnect` | `AgentDisconnectRequest` with force and optional expected plan revision | `AgentChangeResponse` | CLI and desktop disconnect |
| `GET /v1/hofund/usage?period=...` | `UsageQuery` | `UsageResponse` with rows and totals | CLI `usage`, desktop Usage page |
| `POST /v1/hofund/token/rotate` | `TokenRotateRequest` | `TokenRotateResponse` containing the new token once plus per-agent results | CLI `token rotate`, later desktop |
| `GET /v1/hofund/events` | SSE with optional `Last-Event-ID` | `text/event-stream` of `EventEnvelope` | CLI event tail, desktop, future TUI |
| `POST /v1/hofund/shutdown` | `ShutdownRequest` | `ShutdownResponse` acknowledging drain | Desktop Stop and CLI explicit shutdown |

There is deliberately no control `token show` route. Local `token show` reads the same file through config and secrets. `ConfigResponse.text` is the full TOML because L edits whole documents; it can contain credentials and is returned only over an authenticated current-user channel. It is never included in events, logs, status, or error details. Errors use 400 for malformed requests, 401 for authentication, 403 for peer, Host, or Origin refusal, 404 for unknown ids, 409 for revision and restart-required conflicts, 422 for validation, 429 only when an upstream supplied that result, 500 for internal failures, and 503 when the daemon is stopping or unavailable.

## Transports and discovery

### Separate loopback listener

Use a separate control listener. Do not mount `/v1/hofund/*` on the model gateway. The model listener accepts any configured `SocketAddr` today (`crates/config/src/lib.rs:23-29,447-455`), while management must never follow it onto a LAN. Separate listeners also keep model authentication's query-key compatibility away from management and allow management Host and Origin rules to be strict.

The default control TCP address is `127.0.0.1:3426`; an explicit IPv6 choice is `[::1]:3426`. Startup rejects every non-loopback control address. On Unix, the default is the per-config Unix socket, with TCP enabled only when configured or when the socket cannot be used. Windows prefers the named pipe with the same TCP fallback. A single daemon may enable both for migration and testing. The model gateway can still bind a LAN address without exposing a management route.

Use Tokio `TcpListener` and `UnixListener` and `tokio::net::windows::named_pipe` for I/O. Axum routes requests; hyper serves accepted socket and pipe streams through hyper-util's IO adapter. The client uses hyper HTTP/1 connections over the same three stream types. No new heavyweight transport framework is justified.

### Unix socket

For a canonical configuration path, derive `id` as the first 16 lowercase hexadecimal characters of SHA-256 of its UTF-8 bytes. The socket filename is `control-<id>.sock`, so two configurations do not contend for one name.

1. When `XDG_RUNTIME_DIR` is absolute and owned by the current user, use `$XDG_RUNTIME_DIR/hofund/control-<id>.sock`.
2. Otherwise use `/tmp/hofund-<uid>/control-<id>.sock`, including as the short macOS fallback. Create the hofund directory mode `0700` and the socket mode `0600`.
3. Reject a path whose parent is a symlink, has the wrong owner, or is group/world accessible. Remove a stale socket only after connecting fails and no live daemon answers. Never unlink a live or non-socket path. Fail when the platform socket-path limit would be exceeded rather than silently choosing a less predictable location.

The discovery file is `.hofund/control.json` beside the config, written atomically mode `0600`. It contains API version, PID, boot id, canonical config id, bound TCP endpoints if any, and socket or pipe name. It contains no token. Clients verify its file ownership and then verify the daemon with health and authentication.

### Windows named pipe

Use `\\.\pipe\hofund-<current-user-SID>-<config-id>` and a security descriptor equivalent to `D:P(A;;GA;;;<current-user-SID>)`. It is a protected DACL granting access only to the current user, not a default pipe ACL. The server also verifies the connected client's SID before the first HTTP request.

Tokio named-pipe APIs cover pipe I/O. Creating that ACL and checking a client SID requires Windows security APIs. Those APIs need unsafe FFI, while this workspace currently forbids unsafe code (`Cargo.toml:65-67`). No safe wrapper already in the manifest has been verified to provide both operations. L1 must resolve this constraint before claiming Windows named-pipe support: either use a reviewed safe crate with the exact capability or obtain the owner's approval for one tiny audited platform module and a narrowly scoped lint-policy change. If neither is accepted, Windows remains on authenticated loopback TCP and the named-pipe item is incomplete; it must not ship with a default ACL.

### Client discovery order

1. An explicit `--control` endpoint or `HOFUND_CONTROL`, for tests and advanced setups. Only numeric loopback TCP, the expected Unix socket scheme, or the expected Windows pipe scheme is accepted.
2. The authenticated-discovery-file path beside `--config`, checking API version, config id, ownership, and then health.
3. The computed per-config Unix socket or Windows named pipe.
4. The configured control TCP address, defaulting to `127.0.0.1:3426`.

Each failure falls through only for connection refusal, not for a wrong token, wrong protocol version, Host refusal, or wrong peer. Those are actionable errors and trying another endpoint could hide a security problem.

## Authentication and browser-request defenses

Use the same `TokenFile` as the model gateway. TCP requires exactly `Authorization: Bearer <token>`; query tokens and the model gateway's alternate API-key headers are not accepted. Re-read the token file on each request and compare in constant time, preserving the current last-good behavior on an unreadable or empty file (`crates/gateway/src/lib.rs:541-555,596-607`). Never derive or print a `Debug` containing a token, put it in a URL, or include it in tracing, errors, discovery files, SSE, or DTO snapshots.

Require both bearer authentication and a same-user peer check on Unix sockets and Windows pipes. Linux uses `SO_PEERCRED`; macOS uses `getpeereid`; Windows uses the pipe client SID. Peer lookup failure closes the connection. These checks complement, rather than replace, the 0600 socket, 0700 directory, and current-user pipe ACL. Their safe implementation is part of the same FFI decision described above.

Rotation calls the existing atomic replacement operation (`crates/secrets/src/lib.rs:102-113,126-169`). The rotation request and agent-file updates complete before the response returns the new token. Subsequent model and control requests require it; old tokens fail. Existing SSE connections close with a `token_rotated` control event and reconnect using the new token. A token file changed outside the daemon takes effect on the next ordinary request, as it does today. Every SSE connection also re-reads the token on its 15-second heartbeat and closes when the token changes, so an external rotation cannot leave an indefinitely authorized stream. Token-path changes remain restart-required.

Control TCP is loopback-only even when the model gateway binds a LAN address. Apply these HTTP checks before routing:

- `Host` must be the exact numeric loopback host and bound port used by the listener. Reject DNS names, including `localhost`, so DNS rebinding cannot turn a browser into a management client.
- Do not emit CORS permission headers. Reject a cross-site `Origin`; accept only an exactly matching numeric-loopback origin when one is present. CLI and native clients omit Origin. A later webview origin needs an explicit reviewed allowlist entry, not a wildcard.
- Mutations require `Content-Type: application/json` and a bounded body. Form posts and simple cross-origin content types are rejected. Authentication is never cookie-based.

## Event stream

Use SSE, not WebSocket or NDJSON. SSE already matches HTTP, needs no upgrade or additional locked crate, supports `Last-Event-ID`, and is sufficient for one-way daemon-to-client updates. Each frame has `id: <boot-id>:<sequence>`, an event name, and one JSON `EventEnvelope`. A comment heartbeat is sent every 15 seconds. Every event contains API version, boot id, sequence, Unix-millisecond time, and a non-exhaustive payload variant.

| Event | Payload and use |
| --- | --- |
| `snapshot` | Current status, config revision and generation, provider/member summary, and latest sequence. Sent first on every connection. |
| `config_reloaded` | New revision and generation, changed member ids, and restart-required fields, which are empty after a successful apply. No TOML or secrets. |
| `config_rejected` | Sanitized field issues and rejected revision. The old generation remains. |
| `member_cooled` | Member id, stable wire cause, retry or remaining milliseconds, and failure count. No ProviderError or policy type. |
| `member_recovered` | Member id and new readiness. |
| `snapshot_published` | New generation and route/member counts. Panic-driven publication uses this too. |
| `request_started` | Request id, route id, inbound protocol, and start time. No body, headers, URL query, or credential. |
| `request_finished` | Request id, selected member when known, status class, duration, and token counts when known. This is the usage-event subset of rows 11.2 and 12.3, not request capture. |
| `models_refreshed` | Provider id, list kind, model count, success or sanitized failure. |
| `agent_changed` | Agent id, action, changed paths, and warnings without token material. |
| `sign_in_progress` | Reserved additive event for H; no L producer is required. |
| `token_rotated` | Notification only; clients reconnect with the new token. |
| `shutdown_started` | Drain state. |
| `resync_required` | Current snapshot after lag, unknown boot id, or an unreplayable Last-Event-ID. |

A single bounded Tokio broadcast channel holds 256 events. Slow clients do not block model traffic or other clients. On `Lagged`, that client receives `resync_required` and a fresh snapshot; it must discard local derived state. There is no unbounded per-client queue.

`Last-Event-ID` is supported only within one boot's bounded retention. A matching retained id resumes after it. An unknown boot id, an id older than retention, or a malformed id produces `resync_required`, never silent loss. Events are not persisted across daemon restarts. Raw log lines are not events in L because log redaction and volume are separate concerns. Quota history and detailed routing explanations remain later surfaces; H can add quota payloads without changing existing variants.

## Reload semantics

### Triggers and transaction

L supports `POST /config/reload` and SIGHUP on Unix. Both reload the current file. L does not watch the file automatically: editor rename sequences and partial saves can create duplicate or transient invalid candidates, while an explicit apply already provides a transactional path. This still delivers feature-parity row 11.2 because changes apply without a process restart (`docs/feature-parity.md:277`).

`POST /config/apply` accepts a whole TOML document and an expected revision. Serialize reload and apply through one async mutex. Compute the candidate's content revision and reject a stale expected revision with 409. Validation performs the same structural checks and provider construction/preflight as current `config check`, but sends no model probe (`crates/daemon/src/lib.rs:67-76`). Candidate construction happens beside the running snapshot and cannot mutate the current `ModelsHost`, event generation, refresh job, or cooldown book.

After complete validation, atomically write the TOML beside the config and then publish the fully built candidate. A parse, preflight, file-write, or build failure returns an error, emits `config_rejected`, and leaves both the old file's last-good contents and old runtime snapshot in place. Publication and generation increment are infallible once the write and build succeed. A restart-required difference rejects the whole apply before writing, so disk and runtime never silently diverge.

### Reloadable and restart-required fields

| Fields | Rule |
| --- | --- |
| Providers, routes, failover, subscriptions, model-list settings, endpoint and pooled-client timeouts | Reloadable. Environment-backed secrets and options are resolved again during candidate construction. |
| Model `listen` | Restart-required because the bound model `TcpListener` is created at startup (`crates/daemon/src/lib.rs:224-227`). |
| `token_file` | Restart-required so client discovery and every server keep one unambiguous authentication source. Token contents remain live without reload. |
| `usage_file` | Restart-required in L. Moving an open ledger mid-request needs a separate ownership design. |
| Control listener, socket, pipe, and discovery settings | Restart-required. |
| Plugin registry contents and compiled agent descriptions | Restart-required and normally a new binary, not TOML reload. |

In-flight requests retain their old `Arc<Router>` and its owned membership table; new requests load the new ArcSwap snapshot (`crates/router/src/snapshot.rs:46-58`). L initially rebuilds all plugin instances for a valid candidate rather than risking reuse with a stale option or secret. The old snapshot retires after its last strong reference drops, and retired instances receive one bounded asynchronous `shutdown`. Instance reuse for an unchanged build fingerprint is a later optimization, not a correctness requirement.

Stable `MemberId` values, not vector positions, carry cooldown identity across reload. Moving the same provider between routes preserves its id; renaming a provider creates a new id and allows the old entry to be pruned. The candidate membership table and all its published routers receive the existing shared book. Model refresh state remains host-owned and persistent, while the refresh loop is replaced with the candidate's timing and instances.

## Issue #117 and cooldown lifetime

Issue #117 is a prerequisite inside L, not a follow-up. Hoist `Arc<Cooldowns>` out of `router()` into the daemon runtime state before reload lands. Every startup, reload, panic publication, and recovery publication uses that one book. Today only membership republishes share it; a second `router()` call would create a fresh book (`crates/daemon/src/lib.rs:268-285`; `crates/daemon/src/membership.rs:205-226`).

Pruning follows these rules:

1. After a successful reload, remove entries whose `MemberId` is absent from the candidate. Never prune on rejected validation.
2. Success never inserts a missing entry. `record_success` and `record_success_if_unchanged` currently call `states.entry(...).or_default()` (`crates/router/src/cooldown.rs:228-260`); change them to mutate only an existing entry and remove it when the clear leaves no ladder history. Preserve the revision guard by drawing revisions from one book-wide monotonic counter instead of a per-entry counter. Removal and recreation would otherwise restart an entry at revision 1, and an in-flight call that captured revision 1 before the removal could clear a newer failure.
3. Prune expired idle entries on reads, outcome recording, reload, and a periodic maintenance pass. An expired rate-limit entry remains only for the configured `rate_forget` window; generic ladder history expires after the same idle interval unless a longer active rest exists. Active cooldowns are never expired by maintenance.
4. Bound the book at 10,000 entries. Evict expired entries first, then the least-recently touched idle entry. Never evict an active rest to admit an idle entry. Configured members removed on reload do not consume the budget.
5. Recovery emits `member_recovered` once when an active rest expires or is cleared, not on every read.

Issue #120 is resolved in H, not L. It concerns whether `QuotaObservation::retry_after` is an HTTP hint or a window reset and whether a used-share threshold should rest a member. The current proto field is documented as a retry hint (`crates/plugin-api/src/records.rs:171-195`), while the cooldown observes only `remaining == 0` and applies the observation cap (`crates/router/src/cooldown.rs:267-292`). L does not reinterpret that field or change the threshold. It only maps already-decided cooldown state to wire DTOs. H must resolve the semantics before adding subscription producers.

Tests for #117 cover cooldown survival across reload and panic publication, removed-member pruning, expired-entry pruning, success against a missing id without insertion, the 10,000-entry bound, revision-guard behavior, and one recovery event per rest.

## `hofund serve` exec and release

On Unix, `hofund serve` discovers `hofundd` beside `std::env::current_exe()` first, then searches `PATH` using the platform executable suffix. It invokes the selected path directly with `--config <path>`, inherits the environment, and uses `CommandExt::exec`. There is no shell and no parent process left to forward signals: the daemon receives them and its exit status is the command's status. `HOFUND_CONFIG` and provider `env:` values pass through unchanged; an explicit `--config` is passed exactly once.

Windows has no exec replacement. The CLI spawns `hofundd.exe`, waits, and returns the child's numeric exit code. The child stays in the same console so Ctrl-C is delivered to it; the parent waits rather than exiting ahead of graceful shutdown. A child ended without a numeric code maps to failure. A missing binary produces an actionable error naming both searched locations and the required package layout.

`hofundd` owns Tokio runtime construction, startup logging, SIGHUP, Ctrl-C, and graceful drain. The in-process `serve_blocking` remains until the CLI switch so existing desktop and CLI behavior keeps working.

Release changes are part of the switch pull request, not a later afterthought. `dist-workspace.toml` currently builds only `hofund`, and the release document says so (`dist-workspace.toml:5-37`; `docs/release.md:22-38`). The CLI archive and shell/PowerShell installers must contain both `hofund` and `hofundd` in one directory. `hofund-daemon-bin` opts into dist, macOS universal repacking includes both Mach-O binaries, checksums and signing cover both, and Windows packaging includes `hofundd.exe`. The dry-run must build actual artifacts for every release target, not only run `dist plan`, because the current process distinguishes those checks (`docs/release.md:98-121`). Desktop packaging gains no daemon sidecar in L; its later rework owns that decision.

## Ordered implementation pull requests

Each pull request branches from current main, keeps its own changelog and architecture/parity updates, and runs formatting, targeted clippy and tests, `workspace_invariants`, `check-plugins`, and warning-free workspace rustdoc. Each runs only mock upstreams. No live vendor call is a gate.

| PR | Change and compatibility | Parallel with |
| --- | --- | --- |
| L1 | Add control-proto DTOs, version/error mapping, JSON and public-API snapshots. Resolve the safe same-user/Windows ACL implementation question with a minimal platform spike, without shipping a weak pipe fallback. | E, F, G, S; proto-only and disjoint. |
| L2 | Introduce `DaemonRegistry` and daemon runtime state. Hoist and prune the cooldown book for #117. Keep the existing registry constructor and every in-process caller working. | E and S need coordination on registry call sites; F and G are independent. |
| L3 | Add gateway control routing, separate loopback TCP, token re-read, constant-time Bearer auth, Host/Origin defenses, health, version, and status. Add control-client TCP. | E, F, G, S after L2. |
| L4 | Add Unix socket and Windows pipe transports, discovery file, peer checks, and client discovery. TCP remains available on every OS. | Platform-separated work can proceed in parallel; merge as one transport contract. |
| L5 | Add read/action routes for providers, #28 list and probe operations, route models, agents, usage, token rotation, and shutdown. Existing CLI and desktop still use in-process paths. | E and H coordinate only on daemon operation interfaces. |
| L6 | Add serialized validate/apply/reload, SIGHUP, candidate construction, ArcSwap publication, refresh-loop replacement, event bus and SSE. | H may use the event envelope but does not wait for L. |
| L7 | Add `apps/hofundd`, move the production registry link, add `hofund serve` exec while retaining the old path behind a temporary compatibility command, and update two-binary release artifacts and release-path CI. | Not parallel with E or S registry-manifest edits; F and G are independent. |
| L8 | Switch CLI runtime commands to control-client, remove compatibility daemon/registry edges, and enforce the transitive registry ban through cargo metadata. Desktop remains embedded until its follower. | After L1-L7; desktop rework starts only after merge. |

## Test plan by pull request

| PR | Required tests |
| --- | --- |
| L1 | Serde round trips and committed JSON snapshots for every request, response, error, and event; unknown response fields and unknown enum variants; request unknown-field rejection; body and text limits; version mismatch; no plugin/router type in the public API snapshot; stable ProviderError and cooldown mapping including unknown future kinds. |
| L2 | Registry fake proves daemon has no registry dependency. Cooldown survives repeated snapshot publication and candidate reload; removed member pruned; expired entry pruned; success does not insert; bounded eviction preserves active rests; revision races and single recovery events. Existing daemon and CLI tests stay green. |
| L3 | Client against an in-process control server over TCP on all CI OSes. Health is open; every other route rejects no token, wrong token, malformed Bearer, query token, and alternate key header. Wrong Host and DNS-rebinding Host fail; cross-site Origin and form mutation fail. Rotation takes effect on the next request and tokens never appear in Debug or logs. |
| L4 | Unix socket round trip and permissions on Unix CI, stale-socket safety, long-path refusal, wrong-peer rejection through an injected credential test double, and discovery fallback. Named-pipe round trip, current-user-only ACL, wrong-SID rejection, and discovery on the Windows CI runner after the FFI question is resolved. No live upstream is involved. |
| L5 | Every route's success and error envelope against mock daemon operations and mock upstreams. #28 probe uses only a mock upstream and verifies its exact request. Agent plan revisions prevent stale apply; token rotation updates connected agents and closes SSE; shutdown drains. No token show route exists. |
| L6 | Bad TOML, invalid provider, stale revision, and write failure reject without changing file, snapshot, generation, or cooldown. Restart-required fields reject before write. Cooldown survives reload; removed member pruned; in-flight request finishes on the old snapshot while a new call uses the new one; SIGHUP follows the same path. SSE covers initial snapshot, Last-Event-ID resume, reboot resync, and a 256-event lag causing `resync_required` without blocking the producer. |
| L7 | Unix exec test uses a temporary same-directory fake `hofundd` to prove exact config argument, environment, and process replacement; PATH fallback works. Windows spawn/wait test proves exit-code propagation and console behavior on Windows CI. Release dry-run archives on all targets contain and, where configured, sign both binaries; same-directory discovery works from the unpacked archive. |
| L8 | CLI integration tests prove runtime commands no longer link daemon or registry, full config validation uses the server, explicit offline check reports structural-only validation, local token show still works, and every control command uses a mock in-process server. `workspace_invariants` and cargo metadata enforce the final edge set and only-`hofundd` registry link. |

The later desktop follower adds its own client integration and proves a UI crash does not interrupt an in-flight model request. L does not claim that desktop evidence.

## Reference parity mapping

The reference checkout contains several different mechanisms. They are not all one control plane. This table assigns each an L disposition without claiming that an unsearched feature is absent.

| Reference behavior and source | Hofund L equivalent | Disposition and reason |
| --- | --- | --- |
| CLI provider listing reads `provider.All()` and related local state directly (`internal/cli/providers_cli.go` lines 189-240). | CLI provider operations call the running daemon through typed routes. | Improvement: one runtime owner and one validation path replace separate process-local views. |
| GUI serves a small JSON API and static assets from its own handler (`internal/gui/api.go` lines 828-847), with agent connect/disconnect actions at lines 1101-1133 and settings GET/POST at lines 1210-1233. | `/v1/hofund/agents*` and `/config*` in `hofundd`. | Improvement: the UI is a client rather than a second composition root. |
| When another process serves the gateway, the web page shows settings and accounts read from the same files (`internal/gui/web.go` lines 124-128); the backend also watches and can start an embedded gateway (`internal/gui/backend.go` lines 76-97,114-161). | The running daemon owns live state and publishes events. | Improvement: clients do not infer runtime state from files or race to become the gateway. |
| TUI constructs state from agent, provider, settings, and usage packages and runs an embedded gateway when needed (`internal/tui/tui.go` lines 20-28,152-194; `internal/tui/gateway.go` lines 28-78). | Future TUI uses control-client. | Deferred: L defines the client boundary but does not build the TUI. |
| `settings.Load` and `LoadChecked` read the settings file, while `Shared` uses filememo-backed shared parsed state (`internal/settings/settings.go` lines 1185-1228). | Versioned config DTOs and a daemon-owned current snapshot. | Improvement: runtime snapshots are explicit and requests do not each reread configuration. |
| The gateway watches settings, provider, plugin, and access files (`internal/gateway/gateway.go` lines 618-624); `filememo.Watch` watches parent directories and falls back after watcher errors (`internal/filememo/watch.go` lines 33-80). | Explicit reload, transactional apply, and SIGHUP; no automatic watcher in L. | Divergence: fewer duplicate transient candidates and a clear validation result. A later watcher can call the same reload operation. |
| `middleware.Reload` clears the loaded JavaScript middleware set so the next request loads it again (`internal/middleware/middleware.go` lines 92-123). | No JavaScript middleware reload in L. Candidate provider construction and ArcSwap publication replace the running route snapshot. | Divergence: this is a different subsystem, not evidence of whole-config reload. |
| Reserved `/_magpie/` callback and page paths are kept on the gateway port rather than passed through the web page (`internal/gateway/oneport.go` lines 32-48). Request counting and key limiting exempt that prefix (`internal/gateway/busy.go` lines 79-85; `internal/gateway/key_limit.go` lines 36-43). | Management paths live on a separate listener under `/v1/hofund/*`. | Improvement: management cannot collide with agent routes or inherit model billing/counting exemptions. Reference-specific callback paths are not renamed or claimed by L. |
| `/v1/magpie/*` serves quotas, usage, route, trace, concurrency, limits, and profiling beside model routes (`internal/gateway/gateway.go` lines 708-806). | Versioned management routes cover status, usage, providers, models, agents, config, and events. | Improvement for the L subset: management is isolated from a LAN model listener. Quota history, detailed route/trace views, key limits, and profiling are Deferred to their owning rows. |
| `ServedBy` probes the gateway root for identity and version (`internal/gateway/gateway.go` lines 293-341), and `healthcheck` performs a bounded HTTP check (`internal/cli/healthcheck_cli.go` lines 16-31). | `/v1/hofund/health` and `/status`, plus CLI healthcheck and discovery validation. | Same purpose, Improvement: protocol and config identity are checked in addition to process reachability. |
| Unix update handoff uses a private socket, pointer file, peer uid check, SCM_RIGHTS, and acknowledgement (`internal/handoff/handoff.go` lines 1-12,31-83; `internal/handoff/handoff_unix.go` lines 33-85,127-183,197-275). Other platforms report unsupported (`internal/handoff/handoff_other.go` lines 12-41). | None in L. | Deferred: row 16.3 owns listener transfer and drain. The L control socket does not transfer the model listener. |
| Peer identity uses `SO_PEERCRED` on Linux and `LOCAL_PEERCRED` on macOS (`internal/handoff/peer_linux.go` lines 5-13; `internal/handoff/peer_darwin.go` lines 5-13). | Same-user peer verification on local IPC plus bearer auth. | Same security goal, Improvement: peer identity and rotating bearer credentials are both required. |
| Service install supports user systemd and launchd managers (`internal/daemon/daemon.go` lines 1-7,44-74,86-112), writes systemd units (`internal/daemon/systemd.go` lines 14-78) and launch agents (`internal/daemon/launchd.go` lines 11-80), and exposes daemon commands (`internal/cli/daemon_cli.go` lines 123-227). | None in L beyond making `hofundd` a suitable service executable. | Deferred: row 16.4 owns installation, lifecycle commands, environment policy, and service files. |

## Open questions for the owner

1. **Windows ACL and peer-credential FFI:** approve one small audited platform module and a narrowly scoped unsafe-lint exception if no safe crate provides the exact operations? Recommendation: prefer a reviewed safe dependency; otherwise approve the narrow exception. Never substitute the default pipe ACL, and keep Windows on authenticated loopback TCP until this is resolved.
2. **Whole-document configuration apply:** confirm full TOML replacement with `expected_revision` rather than field patches in L? Recommendation: yes. It preserves unknown-to-client fields only when the client starts from GET, detects concurrent editors, and avoids a second patch language. Redacted structured editing can be added later.
3. **Automatic file watching:** confirm explicit apply/reload and SIGHUP without an initial watcher? Recommendation: yes. It satisfies live application without reacting to partial editor writes. Add a debounced watcher later only if user evidence requires it.
4. **Offline `config check`:** accept that full validation requires `hofundd`, with an explicitly named offline structural-only mode? Recommendation: yes. Do not pretend structural parsing performed factory preflight, and do not retain a daemon/registry link just to hide the distinction.
5. **Plugin instance reuse:** confirm rebuild-all for the first reload implementation? Recommendation: yes. Reuse by a canonical build fingerprint can follow once retirement and shutdown behavior is proven; correctness and secret freshness come first.
6. **Socket handoff and background services:** confirm both remain outside L despite the broader sentence in RFC 0001's placement table? Recommendation: yes. They are independent row 16.3 and 16.4 projects; `hofundd` and the client boundary are the only prerequisites L needs to supply.
7. **Reference pin:** accept `dc1cfe0` as the reference for these parity citations, rather than re-deriving them at the RFC pin `cbbf316`? Recommendation: yes. The listed files changed between the pins, so the newer behavior is the parity target. The RFC pin stays unchanged for presets and earlier rows.
