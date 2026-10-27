# Control plane and hot reload

This is the plan for row L of RFC 0001. It covers the management protocol, its clients and transports, the `hofundd` process boundary, live configuration reload, and the event stream. It is a design document only. No behavior or parity status changes with this document.

RFC 0001 assigns L the control crates, `/v1/hofund/*`, token-authenticated loopback and local IPC, an event stream, `apps/hofundd`, the registry-link move, `hofund serve` exec, and snapshot reload (`docs/rfc/0001-microkernel-plugins.md:695-706,776-784,843-857`). The review resolutions require `hofundd` to be the only direct registry linker and the daemon library to receive the registry as an argument (`docs/rfc/0001-microkernel-plugins.md:1097,1112-1116`). The implementation must also close the control-plane parts of feature-parity rows 11.2 and 12.3 (`docs/feature-parity.md:277,288`).

Reference citations in this plan are against yetone/magpie `dc1cfe015675e8c3529cb67b6de3c40991ef2c13`, which is newer than the RFC pin `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`. Between the two pins, these cited files changed: `internal/cli/providers_cli.go`, `internal/gateway/busy.go`, `internal/gateway/gateway.go`, `internal/gateway/oneport.go`, `internal/gui/api.go`, `internal/gui/backend.go`, `internal/gui/web.go`, `internal/middleware/middleware.go`, and `internal/settings/settings.go`. `internal/filememo/watch.go` does not exist at the RFC pin. Every other cited file is unchanged between the pins.

## Scope and non-goals

L delivers one long-running daemon that serves the model gateway and a separate management listener. The CLI uses that management API for runtime operations. `hofund serve` starts `hofundd`. Configuration changes validate and publish without interrupting in-flight model calls. Clients receive lifecycle and request-summary events.

L includes the stable DTO crate, transport client, loopback TCP, Unix domain socket, Windows named pipe, token and same-user checks, health and status, configuration editing and reload, provider and model operations, agent operations, usage, token rotation, events, and graceful shutdown. It includes the cooldown lifetime fix from issue #117 and release packaging for two CLI binaries.

The following wait for later pull requests:

- Desktop removal of its embedded daemon. RFC 0001 explicitly makes this a follower of L (`docs/rfc/0001-microkernel-plugins.md:703-706`).
- A TUI and a browser UI. They can later use the same client, but L adds neither application. A later browser UI requires a reviewed desktop-webview origin allow-list or short-lived token exchanged over an authenticated current-user channel; it must not add cookies, a wildcard origin, or an ad hoc bypass to L's browser defenses.
- An optional authentication-failure rate limit on control TCP. This is later defense in depth against log spam, not a substitute for the high-entropy bearer token.
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

The gateway crate owns axum and hyper serving because it already owns the HTTP edge (`crates/gateway/src/lib.rs:19-37,176-196`). The daemon supplies a control-state interface and DTOs; the gateway maps HTTP to that interface. `control-client` uses hyper and hyper-util for TCP and local IPC, not reqwest, because one connection implementation covers all three transports. The architecture's HTTP rule must therefore continue to allow axum only in gateway while explicitly allowing hyper, http, http-body-util, and hyper-util in gateway and control-client. These crates are already locked: axum 0.8.9, hyper 1.12.0, http-body-util 0.1.5, hyper-util 0.1.21, Tokio 1.54.1, and serde 1.0.229 (`Cargo.lock:137-140,1892-1895,1912-1915,1937-1940,1975-1978,3720-3723,4551-4554`). No SSE or WebSocket dependency is needed. L6 adds `notify` to the daemon for parent-directory watching; it is not currently in `Cargo.lock`. `notify` is CC0-1.0; its inotify backend is ISC, objc2-core-services is `Zlib OR Apache-2.0 OR MIT`, and windows-sys 0.61.2 is already locked, so no new duplicate versions are expected (`Cargo.lock:5315-5322`; `deny.toml:21-43`). Because `notify` logs through the `log` crate, `hofundd` also adds the MIT-licensed `tracing-log` bridge and installs `LogTracer` so watcher diagnostics are retained. L6 runs `cargo deny check` for both additions and their backends. The 2 s stat-polling fallback needs no dependency, so it remains the parity floor if the `notify` addition fails review.

### Exact architecture-test changes in the code pull requests

The design PR changes only the inspiration allowlist. The implementation pull requests make the following changes to `MEMBERS` and `expected_deps` in `xtask/tests/workspace_invariants.rs` and to the normative table and diagram in `docs/architecture.md`.

1. Add `apps/hofundd` immediately after `apps/desktop/src-tauri` in `MEMBERS`. Add `crates/control-client` and `crates/control-proto` immediately after `crates/config`. Add the same members to the root workspace list and add sorted workspace dependency entries for `hofund-control-client` and `hofund-control-proto`.
2. Add `"hofund-control-client" => &["hofund-control-proto"]` and `"hofund-control-proto" => &[]`.
3. Add `"hofund-daemon-bin" => &["hofund-daemon", "hofund-plugin-host", "hofund-plugin-registry"]`.
4. Add `hofund-control-proto` to the existing gateway and daemon lists. Remove `hofund-plugin-registry` from the daemon list in the registry-switch pull request. Do not add a registry edge to either control crate.
5. At the CLI switch, set `"hofund-cli" => &["hofund-config", "hofund-control-client", "hofund-secrets"]`. `expected_deps` lists only workspace crates; `toml_edit` is an external dependency declared through `[workspace.dependencies]`. Config supports the explicitly labeled structural-only fallback when `hofundd` is absent, secrets supports local `token show`, and `toml_edit` supports format-preserving provider edits. Every full validation and runtime operation uses the client or one-shot `hofundd`; remove the daemon and agent edges. `toml_edit` 0.25.17 is already locked transitively but is not currently a declared workspace dependency (`Cargo.lock:4670-4683`).
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
    pub disk_revision: String,
    pub runtime_revision: String,
    pub generation: u64,
    pub text: String,
    pub pending_restart: Vec<String>,
    pub not_applied: Vec<FieldIssue>,
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
| `GET /v1/hofund/health` | `EmptyRequest` | `HealthResponse` with API and daemon versions | Desktop reachability and optional additional CLI diagnostics; not the container healthcheck target |
| `GET /v1/hofund/status` | `EmptyRequest` | `StatusResponse` with PID, start time, configured and running listener labels, generation, disk and runtime revisions, pending-restart and not-applied fields, in-flight count, shutdown state | CLI `status`, desktop |
| `GET /v1/hofund/config` | `EmptyRequest` | `ConfigResponse` | CLI and desktop editor |
| `POST /v1/hofund/config/validate` | `ConfigValidateRequest` | `ConfigValidationResponse` with issues and fields that would await restart | CLI `config check`, desktop save preview |
| `POST /v1/hofund/config/apply` | `ConfigApplyRequest` | `ConfigResponse` | Desktop save; CLI explicit apply |
| `POST /v1/hofund/config/reload` | `ReloadRequest` with expected revision | `ConfigResponse` | CLI `config reload` and the file watcher use the same internal operation |
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

There is deliberately no control `token show` route. Local `token show` reads the same file through config and secrets. `ConfigResponse.text` is the full TOML because L edits whole documents; it can contain credentials and is returned only over an authenticated current-user channel. It is never included in events, logs, status, or error details. A revision is the SHA-256 content hash of the on-disk file. `StatusResponse`, `ConfigResponse`, and the snapshot event all expose `disk_revision`, `runtime_revision`, `pending_restart`, and `not_applied`; unequal revisions mean drift, while pending fields name restart-only configuration written to disk but not yet active and not-applied issues name live fields whose runtime update failed. Errors use 400 for malformed requests, 401 for authentication, 403 for peer, Host, or Origin refusal, 404 for unknown ids, 409 for disk-revision and stale-plan conflicts, 422 for validation, 429 only when an upstream supplied that result, 500 for internal failures, and 503 when the daemon is stopping or unavailable. Restart-only differences are not conflicts: a successful apply records them in `pending_restart`. An apply whose only failure is model relisten returns 200 with `not_applied` containing a `listen` field issue and the sanitized bind error; the rest of the candidate remains published.

## Transports and discovery

### Separate loopback listener

Use a separate control listener. Do not mount `/v1/hofund/*` on the model gateway. The model listener accepts any configured `SocketAddr` today (`crates/config/src/lib.rs:23-29,447-455`), while management must never follow it onto a LAN. Separate listeners also keep model authentication's query-key compatibility away from management and allow management Host and Origin rules to be strict.

The default control TCP address is `127.0.0.1:3426`; an explicit IPv6 choice is `[::1]:3426`. Startup rejects every non-loopback control address. On Unix, the default is the per-config Unix socket, with TCP enabled only when configured or when the socket cannot be used. Windows prefers the named pipe with the same TCP fallback. A single daemon may enable both for migration and testing. The model gateway can still bind a LAN address without exposing a management route.

Use Tokio `TcpListener` and `UnixListener` and `tokio::net::windows::named_pipe` for I/O. Axum routes requests; hyper serves accepted socket and pipe streams through hyper-util's IO adapter. The client uses hyper HTTP/1 connections over the same three stream types. No new heavyweight transport framework is justified.

### Unix socket

Resolve the configuration with `canonicalize()`, including symlink resolution, then derive `id` as the first 16 lowercase hexadecimal characters of SHA-256 of its UTF-8 bytes. On case-insensitive filesystems, case-normalize the canonical path before hashing. The socket filename is `control-<id>.sock`, so path spelling cannot create two identities for one configuration.

1. When `XDG_RUNTIME_DIR` is absolute and owned by the current user, use `$XDG_RUNTIME_DIR/hofund/control-<id>.sock`.
2. Otherwise use `/tmp/hofund-<uid>/control-<id>.sock`, including as the short macOS fallback. Create the hofund directory mode `0700` and the socket mode `0600` through std permission APIs.
3. Reject a path whose parent is a symlink, has the wrong owner, or is group/world accessible. Never unlink a live or non-socket path. Fail when the platform socket-path limit would be exceeded rather than silently choosing a less predictable location.

### Cross-process locks and discovery

The single-instance lock is `.hofund/daemon-<id>.lock` and the discovery file is `.hofund/control-<id>.json`, both primarily beside the resolved config. Thus two config files in one directory have independent daemons and discovery. Before the stale-socket check, the daemon takes its exclusive advisory lock and holds it for the daemon's lifetime. Only the lock holder may probe and remove a stale socket after a connection fails. A second start leaves the socket alone and reports `already served by PID n` from discovery, matching the identity purpose of the reference's `ServedBy` probe (`internal/gateway/gateway.go:293-341`).

All advisory locks use std `File::lock` and `File::try_lock`, stable since Rust 1.89 and available under the workspace's Rust 1.99.1 minimum (`Cargo.toml:31-39`). Std uses `flock` on Unix and `LockFileEx` on Windows. If std proves insufficient, the alternative is fd-lock 4.0.4, licensed `MIT OR Apache-2.0`; its `windows-sys >=0.52, <0.60` and `rustix ^1` dependencies must be checked against `multiple-versions = "deny"` because the lockfile already contains windows-sys 0.45.0, 0.52.0, and 0.61.2 and rustix 1.1.5 (`deny.toml:21-43`; `Cargo.lock:3511-3521,5297-5322`). The same mechanism locks the stable `<resolved-config>.lock` sidecar for configuration writes. That sidecar is opened and never replaced or renamed; the config file itself is never locked because atomic rename replaces its inode and a Windows lock can block the rename.

If `.hofund/` cannot be created beside a read-only config, the daemon lock and discovery file instead use the same filenames under the first available per-user runtime location, in this order: `$XDG_RUNTIME_DIR/hofund/` when that variable is set to an absolute current-user-owned path; otherwise `/tmp/hofund-<uid>/` on Unix; or `%LOCALAPPDATA%\hofund\run\` on Windows. Unix runtime directories follow the same `0700` ownership and permission rules as the socket directory. The discovery file is written atomically mode `0600` where supported and contains API version, PID, boot id, canonical config id, bound TCP endpoints if any, and socket or pipe name. It contains no token. On Unix, clients verify its file ownership and then verify the daemon with control health and authentication. Windows does not need a separate discovery-file ownership security API: the named-pipe client SID check is the ownership boundary, and authenticated loopback TCP remains the fallback.

### Windows named pipe

Use `\\.\pipe\hofund-<current-user-SID>-<config-id>` and a security descriptor equivalent to `D:P(A;;GA;;;<current-user-SID>)`. It is a protected DACL granting access only to the current user, not a default pipe ACL. The server also verifies the connected client's SID before the first HTTP request.

Tokio named-pipe APIs cover pipe I/O, but creating that ACL and checking a client SID requires Windows security APIs. L1's platform spike is Windows-only. Prefer a maintained safe crate with both exact capabilities. If none suffices, use one isolated `hofund-platform-win` crate with a narrow audited `unsafe_code` allowance, a `SAFETY` comment on every unsafe block, an invariant test proving no other crate opts out of the workspace `forbid` rule, and Windows CI (`Cargo.toml:65-67`). Windows stays on authenticated loopback TCP until that support lands; it never uses a default ACL. Until the pipe lands, the Windows token file is protected by the current user's profile-directory ACL because the existing atomic writer sets mode `0600` only on Unix (`crates/secrets/src/lib.rs:126-169`).

### Client discovery order

1. An explicit `--control` endpoint or `HOFUND_CONTROL`, for tests and advanced setups. Only numeric loopback TCP, the expected Unix socket scheme, or the expected Windows pipe scheme is accepted.
2. `.hofund/control-<id>.json` beside the resolved config, checking API version, config id, the platform ownership rule above, and then control health.
3. The same discovery filename under the runtime locations in the daemon's order: valid `$XDG_RUNTIME_DIR/hofund/`, then `/tmp/hofund-<uid>/` on Unix or `%LOCALAPPDATA%\hofund\run\` on Windows.
4. The computed per-config Unix socket or Windows named pipe.
5. The configured control TCP address, defaulting to `127.0.0.1:3426`.

Each failure falls through only for connection refusal, not for a wrong token, wrong protocol version, Host refusal, or wrong peer. Those are actionable errors and trying another endpoint could hide a security problem.

## Authentication and browser-request defenses

Use the same `TokenFile` as the model gateway. TCP requires exactly `Authorization: Bearer <token>`; query tokens and the model gateway's alternate API-key headers are not accepted. Re-read the token file on each request and compare in constant time, preserving the current last-good behavior on an unreadable or empty file (`crates/gateway/src/lib.rs:541-555,596-607`). Never derive or print a `Debug` containing a token, put it in a URL, or include it in tracing, errors, discovery files, SSE, or DTO snapshots.

Require both bearer authentication and a same-user peer check on Unix sockets and Windows pipes. Unix uses Tokio `UnixStream::peer_cred` and std permissions, with no FFI or unsafe code; L4's Unix half is therefore not blocked by the Windows decision. Windows uses the pipe client SID. Peer lookup failure closes the connection. These checks complement, rather than replace, the 0600 socket, 0700 directory, and current-user pipe ACL.

Rotation calls the existing atomic replacement operation (`crates/secrets/src/lib.rs:102-113,126-169`). The rotation request and agent-file updates complete before the response returns the new token. Subsequent model and control requests require it; old tokens fail. Existing SSE connections close with a `token_rotated` control event and reconnect using the new token. A token file changed outside the daemon takes effect on the next ordinary request, as it does today. Every SSE connection also re-reads the token on its 15-second heartbeat and closes when the token changes, so an external rotation cannot leave an indefinitely authorized stream. A token-path change is written with `pending_restart`; until restart, rotation and re-reads continue using the running path.

Control TCP is loopback-only even when the model gateway binds a LAN address. Apply these HTTP checks before routing:

- `Host` must be the exact numeric loopback host and bound port used by the listener, including `[::1]:port` for an IPv6 listener. Reject DNS names, including `localhost`, so DNS rebinding cannot turn a browser into a management client.
- Do not emit CORS permission headers. Reject a cross-site `Origin`; accept only an exactly matching numeric-loopback origin when one is present, including the IPv6 form. CLI and native clients omit Origin. A later webview origin needs an explicit reviewed allow-list entry, not a wildcard.
- Over a Unix socket or named pipe, the client sends the constant `Host: hofund-control`; the server pins that value rather than applying the TCP numeric-loopback rule.
- Mutations require `Content-Type: application/json` and a bounded body. Form posts and simple cross-origin content types are rejected. Authentication is never cookie-based.

## Event stream

Use SSE, not WebSocket or NDJSON. SSE already matches HTTP, needs no upgrade or additional locked crate, supports `Last-Event-ID`, and is sufficient for one-way daemon-to-client updates. Each frame has `id: <boot-id>:<topic>:<sequence>`, an event name, and one JSON `EventEnvelope`. A comment heartbeat is sent every 15 seconds. Every event contains API version, boot id, topic, per-topic sequence, Unix-millisecond time, and a non-exhaustive payload variant.

| Event | Payload and use |
| --- | --- |
| `snapshot` | Current status, disk and runtime revisions, generation, pending-restart and not-applied fields, provider/member summary, and latest cursor for each requested topic. Sent first on every connection. |
| `config_reloaded` | New disk and runtime revisions and generation, changed member ids, `pending_restart`, and `not_applied`. No TOML or secrets. |
| `config_rejected` | Sanitized field issues and rejected revision. The old generation and last good runtime snapshot remain. |
| `listener_relisten_failed` | The `listen` field issue, sanitized bind error, retained running address, and current revisions and generation. The rest of the candidate remains published. |
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
| `resync_required` | Affected topic and current per-topic snapshot after lag, unknown boot id, or an unreplayable Last-Event-ID. |

The `control` and `requests` topics have separate bounded Tokio broadcast channels, separate replay rings, and independent sequence spaces. A client requesting `?topics=control,requests` receives both streams on one SSE response. Before taking the snapshot, the server subscribes to every requested topic; after sending the snapshot, it drops buffered events at or below that topic's snapshot cursor. This ordering closes the snapshot/subscribe gap without duplicates. Each broadcast holds 4,096 events and each ring retains at most 4,096 serialized events and 4 MiB, evicting oldest entries until both limits hold. Slow clients never block producers or other clients. `Lagged` on one topic emits `resync_required` and a fresh snapshot only for that topic, and the client discards only that topic's derived state. Request traffic therefore cannot lag or evict control events. There is no unbounded per-client queue.

`request_started` and `request_finished` are excluded from the default `control` topic and emitted only for clients requesting `?topics=requests` or `?topics=control,requests`. At 100 requests per second, the two request events per request fill the request topic's event-count window in about 20 seconds without shortening control retention. The L6 load test exercises this rate with a slow subscriber and both per-topic caps.

`Last-Event-ID` is supported only within one boot's per-topic replay rings. For a single-topic subscription its value is that topic's id; for a multi-topic subscription it is a comma-separated list with exactly one `<boot-id>:<topic>:<sequence>` cursor for each requested topic. A matching retained cursor resumes only its topic after that sequence. An unknown boot id, a cursor older than that topic's retention, a missing or duplicate topic cursor, or a malformed id produces `resync_required` for the affected topic, never silent loss. Events are not persisted across daemon restarts. Raw log lines are not events in L because log redaction and volume are separate concerns. Quota history and detailed routing explanations remain later surfaces; H can add quota payloads without changing existing variants.

## Reload semantics

### Triggers and transaction

L6 supports explicit `POST /config/reload` and automatic application of external edits. A `notify` watcher watches the configuration's parent directory and, for a symlinked config, the resolved target's parent directory. Only an event whose cleaned path equals the cleaned config path or its resolved symlink target is relevant, matching the reference's exact-path filter (`internal/filememo/watch.go:45-71`). Debounce runs 400 ms after the last matching event and coalesces rename and partial-save sequences; sibling events neither reset the timer nor trigger reload. This exclusion covers usage SQLite and its `-wal`/`-journal` files, token writes, `.hofund/` discovery, and temporary files, which matters because token and usage paths default beside the config (`crates/config/src/lib.rs:805-815`; `crates/daemon/src/lib.rs:789-798`). If watcher setup fails or the watcher terminates, a 2 s stat-polling fallback takes over. Both paths call the same serialized, validated reload operation. A watcher candidate whose disk content revision equals the runtime revision is skipped, which also absorbs the daemon's own apply writes. An invalid edit emits `config_rejected` and keeps the last good runtime snapshot. The reference likewise watches parent directories and retains a polling path (`internal/gateway/gateway.go:618-624`; `internal/filememo/watch.go:33-80`; `internal/pluginrt/config.go:363-395`), keeps a last good parsed generation (`internal/settings/settings.go:1192-1228`), and used a 2 s `KeepFresh` interval at the earlier pin (`internal/gateway/gateway.go:589`, reference `cbbf3163`). Feature-parity row 11.2 closes only when this L6 watcher and its tests land, not with explicit reload alone (`docs/feature-parity.md:277`).

`POST /config/apply` accepts a whole TOML document and an expected disk-content revision. Serialize apply, explicit reload, watcher reload, and model relisten through one async mutex. Under that mutex, re-read the disk and compare its content hash with `expected_revision`; a mismatch returns 409 and never silently overwrites a hand edit. Validation performs the same structural checks and provider construction/preflight as current `config check`, but sends no model probe (`crates/daemon/src/lib.rs:67-76`). Candidate construction happens beside the running snapshot and cannot mutate the current `ModelsHost`, event generation, refresh job, or cooldown book.

After complete validation, resolve symlinks to the real target and write the client's TOML byte-exact, without reserialization. Create and fsync the temporary file in the target's directory, preserve the symlink, copy the target's mode and owner where possible, atomically rename over the target, and fsync the directory. Apply, reload, and one-shot edits take the same exclusive `<resolved-config>.lock` sidecar lock; apply and one-shot writers hold it across the final disk-revision check and write so a cross-process writer cannot interleave. A parse, preflight, file-write, or build failure returns an error, emits `config_rejected`, and leaves the old runtime snapshot in place. A failed apply also leaves the old file's last good contents untouched; a failed reload does not alter or roll back the external edit, and reports the disk/runtime drift. Publication and generation increment are infallible once the write and build succeed.

### Reloadable, live-relisten, and pending-restart fields

| Fields | Rule |
| --- | --- |
| Providers, routes, failover, subscriptions, model-list settings, endpoint and pooled-client timeouts | Reloadable. Environment-backed secrets and options are resolved again during candidate construction. |
| Model `listen` | Live in L6. For a port change, bind the new address first, then close the old listener; on bind failure, keep serving on the old one. For the same port on a different host, close the old listener, bind the new one, and rebind the old address on failure. The close-first case is necessary because binding `0.0.0.0:p` while `127.0.0.1:p` listens fails with `EADDRINUSE` on Linux. In-flight requests finish on the old listener (`internal/gateway/lan.go:198-237`; `internal/gateway/gateway.go:690-704`). |
| `token_file` | Write with `pending_restart: ["token_file"]`. Token contents at the running path remain live without reload. |
| `usage_file` | Write with `pending_restart`. Moving an open ledger mid-request needs a separate ownership design. |
| Control listener, socket, pipe, and discovery settings | Write with `pending_restart`. |
| Plugin registry contents, compiled agent descriptions, and inbound surfaces returned by `hofund_plugin_registry::surfaces()` | Write with `pending_restart`; registry and surface changes normally require a new binary rather than TOML reload. Surfaces are currently attached at startup (`crates/daemon/src/lib.rs:204-227`). |

Apply validates, writes, publishes, and then relistens under the same mutex; it cannot pre-bind a same-port host change. A relisten bind failure does not reject the rest of the candidate: apply or reload still publishes it, keeps or restores the old listener, returns `not_applied` with a `listen` issue and bind error, and emits `listener_relisten_failed` in addition to `config_reloaded`. If restoring the old address also fails, the daemon logs the fatal error through its error-tolerant writer and exits non-zero so its supervisor restarts it. While the running listener differs from the file, `listen` remains in `not_applied`, not `pending_restart`, and status exposes both configured and running addresses. An explicit `config reload` retries relisten even when disk and runtime revisions match; the watcher's equal-revision skip does not retry it.

For every pending-restart row, apply and reload publish all reloadable changes and retain the old runtime resource for the named fields. On every publish, recompute `pending_restart` as the diff between the running immutable restart-only settings and the current published config rather than accumulating field names; reverting an edit therefore clears it on the next publish. `StatusResponse`, `config_reloaded`, and `snapshot` expose that recomputed set, and none of these differences returns a restart-required conflict. `runtime_revision` identifies the last published config generation, so a successful apply with pending fields or a relisten failure leaves disk and runtime revisions equal; `not_applied` distinguishes the latter. Drift means an unapplied or rejected disk edit, not a pending restart. After restart the pending set is empty.

In-flight requests retain their old `Arc<Router>` and its owned membership table; new requests load the new ArcSwap snapshot (`crates/router/src/snapshot.rs:46-58`). L6 reuses a plugin instance when its canonical build fingerprint is unchanged. That fingerprint is a domain-separated SHA-256 hash of the canonical provider configuration section and resolved secret inputs; neither secret material nor a reversible encoding appears in it. Reuse preserves instance-held learned state such as Mantle's `LearnedRoutes` (`crates/provider/src/mantle.rs:20-25`), whose reference counterpart is process-level learned route state (`internal/gateway/mantle.go:30-36`). Changed instances are rebuilt. Retired instances receive one bounded asynchronous `shutdown` only after the last old `Arc<Router>` drops, so a long stream spanning reload finishes normally.

Stable `MemberId` values, not vector positions, carry cooldown identity across reload. Moving the same provider between routes preserves its id; renaming a provider creates a new id and allows the old entry to be pruned. Each member also has a hashed provider configuration fingerprint covering credentials and `base_url`. On successful reload, clear that member's cooldown entry when the fingerprint changes, so a new key or endpoint starts fresh while unchanged members retain state; never store clear secret material in the fingerprint. This extends the reference's credential-sensitive rest identity (`internal/provider/keys.go:68-85`). The candidate membership table and all its published routers receive the existing shared book. Model refresh state remains host-owned and persistent, while the refresh loop is replaced with the candidate's timing and reused or rebuilt instances.

## Issue #117 and cooldown lifetime

Issue #117 is a prerequisite inside L, not a follow-up. Hoist `Arc<Cooldowns>` out of `router()` into the daemon runtime state before reload lands. Every startup, reload, panic publication, and recovery publication uses that one book. Today only membership republishes share it; a second `router()` call would create a fresh book (`crates/daemon/src/lib.rs:268-285`; `crates/daemon/src/membership.rs:205-226`).

Pruning follows these rules:

1. After a successful reload, remove entries whose `MemberId` is absent from the candidate. Never prune on rejected validation.
2. Success never inserts a missing entry. `record_success` and `record_success_if_unchanged` currently call `states.entry(...).or_default()` (`crates/router/src/cooldown.rs:228-260`); change them to mutate only an existing entry and remove it when the clear leaves no ladder history. Preserve the revision guard by drawing revisions from one book-wide monotonic counter instead of a per-entry counter. The counter starts at 1; 0 means missing and is never assigned. `record_success_if_unchanged` treats a captured 0 with an entry now present as changed, so removal and recreation cannot let stale success clear a newer failure.
3. Prune expired idle entries on reads, outcome recording, reload, and a periodic maintenance pass. An expired rate-limit entry remains only for the configured `rate_forget` window; generic ladder history expires after the same idle interval unless a longer active rest exists. Active cooldowns are never expired by maintenance.
4. Bound the book at 10,000 entries. Evict expired entries first, then the least-recently touched idle entry. Never evict an active rest to admit an idle entry. Configured members removed on reload do not consume the budget.
5. Recovery emits `member_recovered` once when an active rest expires or is cleared, not on every read.

Issue #120 is resolved in H, not L. It concerns whether `QuotaObservation::retry_after` is an HTTP hint or a window reset and whether a used-share threshold should rest a member. The current proto field is documented as a retry hint (`crates/plugin-api/src/records.rs:171-195`), while the cooldown observes only `remaining == 0` and applies the observation cap (`crates/router/src/cooldown.rs:267-292`). L does not reinterpret that field or change the threshold. It only maps already-decided cooldown state to wire DTOs. H must resolve the semantics before adding subscription producers.

Tests for #117 cover cooldown survival across reload and panic publication, removed-member pruning, expired-entry pruning, success against a missing id without insertion, the 10,000-entry bound, revision-guard behavior, and one recovery event per rest.

## `hofund serve` exec and release

On Unix, `hofund serve` discovers `hofundd` beside `std::env::current_exe()` first, then searches `PATH` using the platform executable suffix. It invokes the selected path directly with `--config <path>`, inherits the environment, and uses `CommandExt::exec`. There is no shell and no parent process left to forward signals: the daemon receives them and its exit status is the command's status. `HOFUND_CONFIG` and provider `env:` values pass through unchanged; an explicit `--config` is passed exactly once.

Windows has no exec replacement. The CLI spawns `hofundd.exe`, waits, and returns the child's numeric exit code. The child stays in the same console so Ctrl-C is delivered to it; the parent waits rather than exiting ahead of graceful shutdown. A child ended without a numeric code maps to failure. A missing binary produces an actionable error naming both searched locations and the required package layout.

`hofundd` owns Tokio runtime construction, startup logging, signals, and graceful drain. Under foreground `hofund serve`, SIGHUP means terminal close: stop accepting work, drain in-flight requests, flush state, restore the signal's default disposition, and re-raise it so the supervisor sees termination by SIGHUP, not a 128-plus-signal exit (`internal/cli/signals.go:25-70`; `internal/cli/signals_unix.go:12-23`). Draining before the re-raise is an improvement over the reference, which flushes and exits without draining. As in the reference, a SIGHUP that was already ignored when the process started (`nohup`, a background job, or a service manager that ignores it) stays ignored. Otherwise daemonized mode uses the same shutdown semantics. Ctrl-C and SIGTERM also drain and re-raise on Unix. Windows has no SIGHUP; Ctrl-C and console-close events drain and then exit with status 128 plus the signal number, matching the reference's fallback where a process cannot signal itself (`internal/cli/signals.go:59-71`). Reload is available only through the control call and watcher; L offers no SIGHUP-as-reload or SIGUSR1 reload trigger. Drain logging uses a tracing writer that ignores stdout and stderr write errors such as `EIO` after terminal hangup, and `hofundd`'s serve path contains no `println!` or `eprintln!` that could panic while draining. The in-process `serve_blocking` remains until the CLI switch so existing desktop and CLI behavior keeps working.

Release changes are part of the switch pull request, not a later afterthought. `dist-workspace.toml` currently builds only `hofund`, and the release document says so (`dist-workspace.toml:5-37`; `docs/release.md:22-38`). The CLI archive and shell/PowerShell installers must contain both `hofund` and `hofundd` in one directory. `hofund-daemon-bin` opts into dist, macOS universal repacking includes both Mach-O binaries, checksums and signing cover both, and Windows packaging includes `hofundd.exe`. The dry-run must build actual artifacts for every release target, not only run `dist plan`, because the current process distinguishes those checks (`docs/release.md:98-121`). Desktop packaging gains no daemon sidecar in L; its later rework owns that decision.

## CLI operation without a running daemon

When ordinary discovery finds no daemon, the CLI tries and immediately releases the per-config daemon lock, checking the lock locations in the same order as discovery (beside the resolved config, then the runtime fallbacks). If the lock is free, the command follows its documented no-daemon mode below, normally one-shot `hofundd <op> --config <path>` rather than an implicitly started background service. If the lock is held but discovery is not readable yet, the CLI retries discovery with backoff for up to 5 s, then follows the same documented no-daemon policy if the daemon still is not ready. `events tail` remains the explicit autostart exception; `healthcheck` directly probes the model listener instead. For one-shot operations, the CLI locates the binary by the same same-directory-then-PATH rule as `hofund serve`, passes the operation arguments exactly once, waits, and propagates its exit status. The one-shot process constructs the registry adapter, executes the operation in-process, and exits. It never takes the daemon lock, binds a listener, writes discovery, or starts the watcher. Config writes take the `<resolved-config>.lock` sidecar lock and recheck `expected_revision` before writing. Thus registry linkage remains only in `hofundd`, while agent wiring, provider tests, usage, and full validation still work offline. Only `config check` has a missing-binary fallback: if `hofundd` is not installed, it may perform structural parsing but must print `structural check only; hofundd is unavailable, so provider construction and preflight did not run`. Other one-shot commands report the missing binary as an actionable error.

The reference dispatch and agent forms are at `internal/cli/main.go:383-552`; every provider verb and alias is at `internal/cli/providers_cli.go:420-713`, usage reads local state at `internal/cli/usage_cli.go:42-54`, and healthcheck is dispatched at `internal/cli/main.go:279-280`. Every L command with a reference offline form has this explicit mapping and a named no-daemon test. In this table, "locked edit" means the sidecar-locked read-edit-write operation with an expected-revision recheck:

| Reference command or form | Hofund command | No-daemon mode | Named L8 test with no daemon |
| --- | --- | --- | --- |
| `ls`, `list`, `agents` | `hofund agent list` | One-shot `hofundd agent list` | `offline_agent_list_one_shot` |
| `<agent>` | `hofund agent status <agent>` | One-shot | `offline_agent_status_one_shot` |
| `<agent> <model>` | `hofund agent set <agent> model=<model>`, connecting first when needed | One-shot | `offline_agent_set_model_one_shot` |
| Wired `<agent> default` | `hofund agent disconnect <agent>` | One-shot | `offline_agent_disconnect_one_shot` |
| Unwired `<agent> default` | `hofund agent set <agent> model=default` | One-shot | `offline_agent_clear_model_one_shot` |
| `providers` | `hofund provider list` | One-shot | `offline_provider_list_one_shot` |
| `provider <id>` | `hofund provider show <id>` | One-shot | `offline_provider_show_one_shot` |
| `provider add` | `hofund provider add` | One-shot locked edit | `offline_provider_add_one_shot` |
| `provider set` | `hofund provider set` | One-shot locked edit | `offline_provider_set_one_shot` |
| `provider key` | `hofund provider key` | One-shot locked edit | `offline_provider_key_one_shot` |
| `provider icon` | `hofund provider icon` | One-shot locked edit | `offline_provider_icon_one_shot` |
| `provider fallback` | `hofund provider fallback` | One-shot; editing forms use a locked edit | `offline_provider_fallback_one_shot` |
| `provider rm`, `remove`, `delete` | `hofund provider rm` | One-shot locked edit | `offline_provider_rm_one_shot` |
| `provider test` | `hofund provider test` | One-shot against a mock upstream | `offline_provider_test_one_shot` |
| `provider account-models`, `account-model` | `hofund provider account-models` | One-shot locked edit | `offline_provider_account_models_one_shot` |
| `provider account-cap`, `account-caps` | `hofund provider account-cap` | One-shot locked edit | `offline_provider_account_cap_one_shot` |
| `provider account-concurrency`, `account-limit` | `hofund provider account-concurrency` | One-shot locked edit | `offline_provider_account_concurrency_one_shot` |
| `provider queue` | `hofund provider queue` | One-shot locked edit | `offline_provider_queue_one_shot` |
| `provider rpm` | `hofund provider rpm` | One-shot locked edit | `offline_provider_rpm_one_shot` |
| `provider refresh-every` | `hofund provider refresh-every` | One-shot locked edit | `offline_provider_refresh_every_one_shot` |
| `provider auto-include` | `hofund provider auto-include` | One-shot locked edit | `offline_provider_auto_include_one_shot` |
| `provider quota-rest` | `hofund provider quota-rest` | One-shot locked edit | `offline_provider_quota_rest_one_shot` |
| `provider rest-cap` | `hofund provider rest-cap` | One-shot locked edit | `offline_provider_rest_cap_one_shot` |
| `provider listed` | `hofund provider listed` | One-shot locked edit | `offline_provider_listed_one_shot` |
| `provider off`, `on` | `hofund provider off`, `on` | One-shot locked edit | `offline_provider_off_on_one_shot` |
| `provider models <id>` | `hofund provider models <id>` | One-shot read-only model listing | `offline_provider_models_one_shot` |
| `provider models <id> <ids...>` | `hofund provider models <id> <ids...>` | One-shot locked edit, distinct from read-only listing | `offline_provider_model_picks_one_shot` |
| `provider refresh`, `fetch` | `hofund provider refresh`; `fetch` is an alias of `refresh` | One-shot against a mock upstream | `offline_provider_refresh_one_shot` |
| `models` | `hofund model list` | One-shot | `offline_model_list_one_shot` |
| `model` | `hofund model show` | One-shot | `offline_model_show_one_shot` |
| `usage` | `hofund usage` | One-shot | `offline_usage_one_shot` |
| `healthcheck` | `hofund healthcheck` | No one-shot, token, or discovery. GET the model listener's unauthenticated `/health` at configured `listen`, with `HOFUND_ADDR` overriding the base address, a 3 s timeout, and a bounded body. Require 200 and a hofund identity marker; no daemon answering fails nonzero (`internal/cli/healthcheck_cli.go:16-31`). | `offline_healthcheck_fails_without_daemon` |

L3 adds `"service":"hofund"` to the model `/health` body, which currently returns only `{"status":"ok"}`, so healthcheck can verify identity (`crates/gateway/src/lib.rs:143-151,350-352`). Probing control `/v1/hofund/health` in addition is permitted for diagnostics but never replaces the model probe. `healthcheck_probes_model_gateway_not_control` proves that a live control socket with a dead model listener fails, and `healthcheck_honors_hofund_addr_override` covers the override. This preserves the reference's model-gateway target and 3 s bounded check (`internal/cli/healthcheck_cli.go:20-31`).

The other L runtime commands also have explicit modes and no-daemon tests: `agent connect` and `agent connect --dry-run` use one-shot in `offline_agent_connect_one_shot` and `offline_agent_plan_one_shot`; `config check` uses one-shot full validation in `offline_config_check_one_shot`; `config apply` uses `offline_config_apply_one_shot`; offline `config reload` is a reported no-op that exits 0 with `no daemon running; nothing to reload` in `offline_config_reload_is_reported_noop`; offline `status` prints a local report containing config path, config id, and disk revision, marked `daemon not running`, in `offline_status_local_report`; `shutdown` is a reported no-op in `offline_shutdown_one_shot`; and `token rotate` uses `offline_token_rotate_one_shot`. `events tail` is the one autostart exception: it starts `hofundd`, waits for discovery and health, retries the stream, and proves it in `offline_events_tail_autostarts_and_retries`. `hofund token show` remains entirely local and has `offline_token_show_local`. In particular, `hofund agent disconnect` must succeed with no daemon, including restoring the agent's pre-connect files.

With a running daemon, every provider row marked as a locked edit performs GET, edits the returned text with `toml_edit`, and POSTs the whole document with its disk `expected_revision`. On 409 the CLI discards the stale edit, GETs the new document, reapplies the requested change, and retries up to three times before returning the conflict. This preserves comments through S4's byte-exact write and gives row 11.5 its client-side config surgery. The CLI owns the direct `toml_edit` dependency; proto stays transport-agnostic. One-shot edits hold the sidecar lock and recheck the disk revision; on conflict the CLI rereads and reapplies the operation with the same three-attempt bound.

The remaining reference dispatch entries in `internal/cli/main.go:383-484` are deliberately not assigned fictitious L8 tests. `serve` starts the foreground daemon; help and version stay local; `tui`, `web`, `app`, `gui`, `tray`, `panel`, and `-Embedding` belong to later UI rows; `autostart` and `daemon` belong to rows 15.8 and 16.4. `sync`, profiles (`save`, `use`, `profiles`, and profile `rm`), `import`, `presets`, `visible`, `search`, `groups`, `group`, `gateway-key`, `accounts`, `account`, `sessions`, `quota`, `quotas`, `update`, `library`, `lib`, `backup`, `restore`, `webdav`, `dav`, `plugin`, `plugins`, `s3`, `github`, and `claude-code` belong to their owning catalog, agent, provider, routing, subscription, usage, update, library, backup, sync, plugin, or install-helper rows. Their no-daemon policy is specified when those commands land; none is silently dropped by the L8 registry-edge removal.

## Ordered implementation pull requests

Each pull request branches from current main, keeps its own changelog and architecture/parity updates, and runs formatting, targeted clippy and tests, `workspace_invariants`, `check-plugins`, and warning-free workspace rustdoc. Each runs only mock upstreams. No live vendor call is a gate.

| PR | Change and compatibility | Parallel with |
| --- | --- | --- |
| L1 | Add control-proto DTOs, including `not_applied` and per-topic cursors, version/error mapping, JSON and public-API snapshots. Resolve the Windows-only safe ACL and client-SID implementation question with a minimal platform spike, without shipping a weak pipe fallback or blocking Unix work. | E, F, G, S; proto-only and disjoint. |
| L2 | Introduce `DaemonRegistry` and daemon runtime state. Hoist and prune the cooldown book for #117, including book-wide revisions and provider fingerprints. Keep the existing registry constructor and every in-process caller working. | E and S need coordination on registry call sites; F and G are independent. |
| L3 | Add gateway control routing, separate loopback TCP, token re-read, constant-time Bearer auth, Host/Origin defenses, control health, model `/health` identity, version, and revision/not-applied-aware status. Add control-client TCP. | E, F, G, S after L2. |
| L4 | Add Unix socket and Windows pipe transports, per-config canonical discovery and single-instance lifetime locks, runtime-directory fallback, safe Unix peer checks, and matching client discovery. TCP remains available on every OS. | Platform-separated work can proceed in parallel; merge as one transport contract. |
| L5 | Add read/action routes for providers, #28 list and probe operations, route models, agents, usage, token rotation, and shutdown. Existing CLI and desktop still use in-process paths. | E and H coordinate only on daemon operation interfaces. |
| L6 | Add serialized validate/apply/reload, sidecar-locked byte-exact atomic writes, the exactly filtered debounced watcher and polling fallback, two-case live model relisten with not-applied reporting, recomputed pending-restart reporting, fingerprint-based instance reuse, SIGHUP drain and default-disposition re-raise, error-tolerant logging, ArcSwap publication, refresh-loop replacement, and independent per-topic event buses, replay rings, and SSE cursors. | H may use the event envelope but does not wait for L. |
| L7 | Add `apps/hofundd`, move the production registry link, add `hofund serve` exec while retaining the old path behind a temporary compatibility command, and update two-binary release artifacts and release-path CI. | Not parallel with E or S registry-manifest edits; F and G are independent. |
| L8 | Switch CLI runtime commands to control-client, add daemon-lock detection and one-shot `hofundd` fallback, direct model healthcheck, all provider `toml_edit` surgery, local offline status and no-op offline reload, remove compatibility daemon/registry/agent edges, and enforce the transitive registry ban through cargo metadata. Desktop remains embedded until its follower. | After L1-L7; desktop rework starts only after merge. |

## Test plan by pull request

| PR | Required tests |
| --- | --- |
| L1 | Serde round trips and committed JSON snapshots for every request, response, error, and event, including dual revisions, `pending_restart`, `not_applied`, and per-topic cursors; unknown response fields and enum variants; request unknown-field rejection; body and text limits; version mismatch; no plugin/router type in the public API; stable ProviderError and cooldown mapping. `only_platform_win_may_allow_unsafe_code` proves no other crate opts out of `forbid`; the Windows spike and CI prove the protected pipe DACL and client-SID check or leave Windows on TCP. |
| L2 | Registry fake proves daemon has no registry dependency. Cooldown survives repeated publication and candidate reload; removed member pruned; expired entry pruned; success does not insert; bounded eviction preserves active rests; single recovery events. `stale_success_after_missing_capture_does_not_clear_concurrent_failure` captures 0 while missing, inserts a concurrent failure, and proves stale success cannot clear it. `provider_fingerprint_is_secret_free_and_changes_for_key_and_base_url` covers fingerprint identity. Existing daemon and CLI tests stay green. |
| L3 | Client against an in-process control server over TCP on all CI OSes. Control health and model health are open; every other control route rejects no token, wrong token, malformed Bearer, query token, and alternate key header. `model_health_reports_hofund_identity` covers the healthcheck marker. Wrong Host and DNS-rebinding Host fail; `ipv6_loopback_host_and_origin_exact_match` accepts `[::1]:port`; cross-site Origin and form mutation fail. `status_reports_disk_runtime_revision_pending_restart_and_not_applied` checks the DTO contract. Rotation takes effect on the next request and tokens never appear in Debug or logs. |
| L4 | Unix socket round trip and std-set permissions, safe `peer_cred` same-user verification, long-path refusal, and discovery fallback. `concurrent_start_single_instance_lock` proves the lock is taken before stale-socket cleanup and the loser reports the serving PID without unlinking. `two_configs_in_one_directory_run_independent_daemons`, `read_only_config_dir_falls_back_to_runtime_dir`, and `windows_single_instance_lock_uses_lockfileex` cover per-config identity, fallback, and Windows CI. `canonical_config_id_resolves_symlinks_and_normalizes_case` covers config identity. `local_ipc_host_is_pinned_constant` covers UDS and pipes. `windows_token_uses_profile_acl_until_pipe` runs on Windows CI with the TCP fallback. Named-pipe round trip, current-user-only ACL, wrong-SID rejection, and `windows_discovery_uses_pipe_sid_not_file_owner_api` run on Windows CI when pipe support lands. No live upstream is involved. |
| L5 | Every route's success and error envelope against mock daemon operations and mock upstreams. #28 probe uses only a mock upstream and verifies its exact request. Agent plan revisions prevent stale apply; token rotation updates connected agents and closes SSE; shutdown drains. No token show route exists. |
| L6 | Bad apply TOML, an invalid apply provider, stale revision, and apply write failure leave file, snapshot, generation, and cooldown unchanged. `watcher_debounces_parent_directory_rename_and_partial_save`, `watcher_ignores_sibling_usage_and_token_writes`, `watcher_follows_symlink_target_dir`, `watcher_setup_or_termination_uses_two_second_stat_polling`, `watcher_invalid_edit_keeps_last_good_and_emits_config_rejected`, and `watcher_skips_daemon_write_when_revisions_match` close the automatic-edit cases. `apply_stale_disk_revision_returns_409_without_overwriting_hand_edit`, `one_shot_config_write_uses_sidecar_lock`, and `concurrent_one_shot_edits_serialize_on_sidecar_lock` cover cross-process writers. `atomic_apply_preserves_symlink_mode_owner_and_exact_bytes_and_fsyncs` covers the write contract. `relisten_port_change_binds_new_before_closing_old`, `relisten_same_port_host_change_closes_first_and_restores`, `relisten_failure_publishes_rest_and_reports_not_applied`, `relisten_restore_failure_exits_nonzero`, and `model_relisten_inflight_request_finishes` cover the two relisten algorithms and failure policies. `pending_restart_fields_agree_across_response_status_and_events` and `pending_restart_clears_when_edit_is_reverted` cover restart-only rows. `unchanged_fingerprint_reuses_instance_and_preserves_learned_routes`, `changed_fingerprint_rebuilds_instance`, and `long_stream_finishes_before_retired_instance_shutdown` cover instance lifetime. `provider_fingerprint_change_clears_only_that_cooldown` covers key and `base_url` changes. `sighup_drains_then_reraises_default_disposition` covers foreground and daemonized modes, `sighup_drain_with_closed_stdout_does_not_panic` covers error-tolerant logging, and `sighup_ignored_at_start_stays_ignored` covers `nohup`. SSE tests cover per-topic subscribe-before-snapshot duplicate suppression, bounded Last-Event-ID replay, single-topic resync, reboot resync, opt-in request topics, `control_topic_not_lagged_or_evicted_by_request_load`, and `request_event_load_100rps_slow_client_respects_per_topic_4096_events_and_4mib` without blocking producers. `cargo deny check` passes with notify, tracing-log, and their backends. |
| L7 | Unix exec test uses a temporary same-directory fake `hofundd` to prove exact config argument, environment, and process replacement; PATH fallback works. Windows spawn/wait test proves exit-code propagation and console behavior on Windows CI. Release dry-run archives on all targets contain and, where configured, sign both binaries; same-directory discovery works from the unpacked archive. |
| L8 | Every no-daemon test named in the offline command section starts with no running daemon. One-shot cases invoke the real `hofundd` test binary or mock upstream, including disconnect and full offline config validation; the event-tail case proves autostart, discovery, and retry. `one_shot_never_takes_daemon_lock_or_writes_discovery` also proves no listener bind or watcher start, and `cli_waits_for_discovery_when_daemon_lock_held` covers the bounded startup wait. `healthcheck_probes_model_gateway_not_control` and `healthcheck_honors_hofund_addr_override` cover the direct model probe. `offline_config_check_without_hofundd_is_labeled_structural_only` covers the sole missing-binary fallback. `provider_edit_409_refetches_and_reapplies` and `provider_edit_preserves_comments` cover online surgery. Runtime commands use a mock in-process server when one is discovered; `workspace_invariants` and cargo metadata enforce the final edge set and only-`hofundd` registry link. |

The later desktop follower adds its own client integration and proves a UI crash does not interrupt an in-flight model request. L does not claim that desktop evidence.

## Reference parity mapping

The reference checkout contains several different mechanisms. They are not all one control plane. This table assigns each an L disposition without claiming that an unsearched feature is absent.

| Reference behavior and source | Hofund L equivalent | Disposition and reason |
| --- | --- | --- |
| CLI provider listing reads `provider.All()` and related local state directly (`internal/cli/providers_cli.go:189-240`). | CLI provider operations call the running daemon through typed routes or execute through one-shot `hofundd`. | Improvement: one runtime owner and one validation path replace separate process-local views without requiring a running daemon. |
| GUI serves a small JSON API and static assets from its own handler (`internal/gui/api.go` lines 828-847), with agent connect/disconnect actions at lines 1101-1133 and settings GET/POST at lines 1210-1233. | `/v1/hofund/agents*` and `/config*` in `hofundd`. | Improvement: the UI is a client rather than a second composition root. |
| When another process serves the gateway, the web page shows settings and accounts read from the same files (`internal/gui/web.go` lines 124-128); the backend also watches and can start an embedded gateway (`internal/gui/backend.go` lines 76-97,114-161). | The running daemon owns live state and publishes events. | Improvement: clients do not infer runtime state from files or race to become the gateway. |
| TUI constructs state from agent, provider, settings, and usage packages and runs an embedded gateway when needed (`internal/tui/tui.go` lines 20-28,152-194; `internal/tui/gateway.go` lines 28-78). | Future TUI uses control-client. | Deferred: L defines the client boundary but does not build the TUI. |
| `settings.Load` and `LoadChecked` read the settings file, while `Shared` uses filememo-backed shared parsed state (`internal/settings/settings.go:1185-1228`). | Versioned config DTOs, debounced watch plus explicit reload, and a daemon-owned last good snapshot. | Same behavior for live edits and last good fallback, with explicit disk/runtime revisions and transactional publication instead of per-request reparsing. |
| The gateway watches settings, provider, plugin, and access files (`internal/gateway/gateway.go:618-624`); `filememo.Watch` watches parent directories, filters cleaned event paths, and falls back after watcher errors (`internal/filememo/watch.go:33-80`). | L6 watches config and symlink-target parent directories, filters to those exact cleaned paths, debounces 400 ms, and provides a 2 s stat fallback, transactional apply, and explicit reload. | Same behavior: filtered debounced watch plus explicit reload. Sibling-write exclusion prevents self-triggered reloads, while validation events and revision skipping make failures visible and absorb daemon writes. |
| Relisten binds before closing only for a port change, closes first for a same-port host change, and lets in-flight requests finish (`internal/gateway/lan.go:198-237`; `internal/gateway/gateway.go:690-704`). | L6 uses the same two algorithms, publishes the rest of a candidate after relisten failure, reports `listen` in `not_applied` and an event, retries on explicit reload, and exits non-zero if old-listener restoration fails. | Same behavior for listener migration and in-flight requests; Improvement: partial failure and drift are explicit rather than only an operational error. |
| `middleware.Reload` clears the loaded JavaScript middleware set so the next request loads it again (`internal/middleware/middleware.go` lines 92-123). | No JavaScript middleware reload in L. Candidate provider construction and ArcSwap publication replace the running route snapshot. | Divergence: this is a different subsystem, not evidence of whole-config reload. |
| Reserved `/_magpie/` callback and page paths are kept on the gateway port rather than passed through the web page (`internal/gateway/oneport.go` lines 32-48). Request counting and key limiting exempt that prefix (`internal/gateway/busy.go` lines 79-85; `internal/gateway/key_limit.go` lines 36-43). | Management paths live on a separate listener under `/v1/hofund/*`. | Improvement: management cannot collide with agent routes or inherit model billing/counting exemptions. Reference-specific callback paths are not renamed or claimed by L. |
| `/v1/magpie/*` serves quotas, usage, route, trace, concurrency, limits, and profiling beside model routes, including remote usage ingest, remote quota cards and refresh, and quota/usage reads (`internal/gateway/gateway.go:708-806`). The reference guards these with CORS and caller checks (`internal/gateway/cors.go:44-90`; `internal/gateway/lan.go:411-464`). | Versioned control routes cover the local L subset. When their owning rows land, LAN-facing remote usage ingest, remote quota cards/refresh, and quota/usage reads go on the model listener with gateway-key authentication. | Improvement for the local L subset: control management stays isolated on the loopback listener. The control listener is never widened for LAN features; quota history, detailed route/trace views, key limits, and profiling remain deferred to their owning rows. |
| `ServedBy` probes the gateway root for identity and version (`internal/gateway/gateway.go:293-341`), and `healthcheck` GETs the model gateway with a 3 s timeout and requires 200 and an identity marker (`internal/cli/healthcheck_cli.go:16-31`). | CLI healthcheck GETs the model `/health` at configured `listen` or `HOFUND_ADDR`, with the same timeout and status/identity requirements. Control health and status separately validate discovery and protocol identity. | Same behavior: the container check probes the model gateway without a token or discovery, while control checks add protocol and config identity. |
| Signal handling flushes and re-raises with the default disposition on Unix, without draining in-flight requests (`internal/cli/signals.go:25-70`; `internal/cli/signals_unix.go:12-23`). | SIGHUP, Ctrl-C, and SIGTERM drain, flush, and re-raise on Unix; Windows console events drain and exit with status 128 plus the signal number, like the reference fallback. | Improvement: supervisors still observe signal termination on Unix, and in-flight requests drain before it. |
| Unix update handoff uses a private socket, pointer file, peer uid check, SCM_RIGHTS, and acknowledgement (`internal/handoff/handoff.go` lines 1-12,31-83; `internal/handoff/handoff_unix.go` lines 33-85,127-183,197-275). Other platforms report unsupported (`internal/handoff/handoff_other.go` lines 12-41). | None in L. | Deferred: row 16.3 owns listener transfer and drain. The L control socket does not transfer the model listener. |
| Peer identity uses `SO_PEERCRED` on Linux and `LOCAL_PEERCRED` on macOS (`internal/handoff/peer_linux.go` lines 5-13; `internal/handoff/peer_darwin.go` lines 5-13). | Same-user peer verification on local IPC plus bearer auth. | Same security goal, Improvement: peer identity and rotating bearer credentials are both required. |
| Service install supports user systemd and launchd managers (`internal/daemon/daemon.go` lines 1-7,44-74,86-112), writes systemd units (`internal/daemon/systemd.go` lines 14-78) and launch agents (`internal/daemon/launchd.go` lines 11-80), and exposes daemon commands (`internal/cli/daemon_cli.go` lines 123-227). | None in L beyond making `hofundd` a suitable service executable. | Deferred: row 16.4 owns installation, lifecycle commands, environment policy, and service files. |

## Decisions

1. **Windows ACL and peer credentials — Resolved.** Unix uses safe Tokio peer credentials and std permissions. Windows prefers a maintained safe crate for the protected current-user pipe DACL and client-SID check. If none suffices, one isolated `hofund-platform-win` crate receives the narrow audited unsafe allowance, SAFETY comments, the no-other-crate invariant test, and Windows CI. Windows remains on authenticated loopback TCP until then; a default ACL is never acceptable.
2. **Whole-document configuration apply — Resolved.** L uses full TOML replacement with `expected_revision` compared to the disk content hash under the serialized operation, plus the symlink-preserving, byte-exact atomic write. Cross-process writers share the stable `<resolved-config>.lock` sidecar, never the replaced config inode. Clients start from GET; provider edits use `toml_edit` and 409 retry rather than a second patch language. `pending_restart` is recomputed from running restart-only settings on every publish, so reverts clear it.
3. **Automatic file watching — Resolved.** L6 watches config and resolved-target parent directories, filters events to exact cleaned config or target paths, debounces only matching events for 400 ms, retains the 2 s stat fallback and shared validated reload operation, skips equal-revision watcher candidates, and keeps the last good snapshot on rejection. Row 11.2 cannot close without it.
4. **Offline `config check` — Resolved.** One-shot `hofundd check --config ...` performs full offline validation without moving registry linkage into the CLI. Structural-only validation is permitted only as a clearly labeled fallback when `hofundd` is not installed.
5. **Plugin instance reuse — Resolved.** L6 reuses instances whose canonical provider-configuration and resolved-secret fingerprint is unchanged, preserving learned state. Changed instances are rebuilt, and retired instances shut down only after their last old router reference drops.
6. **Socket handoff and background services — Resolved.** Both remain outside L. They are independent feature-parity rows 16.3 and 16.4 (`docs/feature-parity.md:329-330`); `hofundd` and the client boundary are L's only prerequisites for them.
7. **CLI removal of `hofund-agent` — Resolved.** L8 removes the CLI's daemon and agent edges only together with daemon-lock detection, bounded discovery retry, the one-shot `hofundd` fallback, and every named no-daemon test above. One-shot processes never take the daemon lock, bind, write discovery, or start the watcher; their config writes use the sidecar lock. Offline reload is a successful reported no-op and offline status is a local report. Agent disconnect, provider operations, usage, and full config validation therefore keep working without a running daemon.
8. **Reference pin — Resolved.** L cites reference `dc1cfe0`; earlier rows retain `cbbf316`. The listed changed files and the explicitly marked earlier-pin citation document where behavior differs.
9. **Live relisten and listener failure — Resolved.** Port changes bind before closing; same-port host changes close first and restore on failure. Apply writes and publishes before relistening. A bind failure preserves the rest of publication and reports `listen` through `not_applied` and `listener_relisten_failed`; explicit reload retries even at equal revisions. Failure to restore the old listener is fatal and exits non-zero for supervisor restart.
10. **Lock and discovery identity — Resolved.** Daemon lock and discovery filenames include the canonical config id, with ordered per-user runtime fallback when the config directory is read-only. Std file locking supplies Unix `flock` and Windows `LockFileEx`; clients check discovery locations in the same order.
11. **Healthcheck target — Resolved.** The container healthcheck probes the unauthenticated model `/health` with a 3 s bound and requires 200 plus `"service":"hofund"`; it needs neither discovery nor a token. Control health is optional additional diagnostics, never a substitute.
12. **SSE topic isolation — Resolved.** Control and request topics have independent broadcasts, replay rings, capacities, sequence spaces, and cursors. Multi-topic replay uses per-topic ids, and lag resynchronizes only the affected topic.
13. **Signal termination after drain — Resolved.** Unix drains and flushes before re-raising with the default disposition, which improves on the reference without hiding signal termination from supervisors. Windows console events drain and exit with status 128 plus the signal number, and drain logging tolerates closed stdout and stderr.

No new open questions remain in this plan.
