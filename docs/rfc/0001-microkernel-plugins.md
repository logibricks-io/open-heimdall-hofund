# RFC 0001: Microkernel and plugins

- Status: **Accepted**. Owner sign-off on 2026-10-25 is conditional on the architecture review (appendix), including wording fixes F1, F2, and F3. This document is the design. PR B landed the seam crates. The dependency table in [architecture.md](../architecture.md) includes those edges. Later migration pull requests update it again.
- Date: 2026-10-25. Amended the same day from the architecture review (appendix), and accepted the same day under that review.
- hofund pin: `dfb6e89e64b1eb5d9b489b7f7af2f0685bac41f4` (main after pull request #26)
- Upstream pin: yetone/magpie `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`. `internal/provider/presets.go` is byte-identical at `5b2b3d3f50959735403dff6b77a85eec65772218`. The preset file records that pin at `crates/provider/presets.toml` line 8.
- Workspace version: `0.1.0` (`xtask/tests/workspace_invariants.rs` asserts the manifest). PR B landed the plugin API crate and `const API`. The workspace stays `publish = false`. The semver policy is in section 1.

## Summary

Upstream providers, subscription sign-in, and coding-agent integrations are the three surfaces that several agents need to extend at once. Today they share crates with the gateway spine, so those agents edit the same files.

This proposal keeps the spine as a kernel. The kernel owns mechanism: pooled HTTP, secrets, the account store, the clock, the scheduler, process spawn, tracing, the router snapshot, and a layer chain around every upstream call. Plugins own policy: vendor URLs, signatures, sign-in, and file layouts. They receive a `HostContext` and talk HTTP only through the SDK.

`hofund-plugin-api` is the small contract. `hofund-plugin-sdk` holds helpers. A plugin depends only on those two crates. Plugins do not depend on each other, and they do not open sockets or spawn processes themselves. Stage 1 links plugins at compile time through an explicit registry. In the end state the `hofundd` binary is the only composition root. Desktop and TUI are clients of a versioned local API. A process host for community JavaScript auth plugins is a later stage.

The trait blocks in this document are the proposed contract. This pull request does not compile them.

## 1. Kernel boundary

### What the kernel is today

The spine, from [architecture.md](../architecture.md), is `ir <- codec <- provider <- router <- gateway <- daemon <- cli`. `agent` depends only on `config`. `apps/desktop` sits beside `cli` and depends on `daemon` and `agent`. `xtask/tests/workspace_invariants.rs` function `expected_deps` (line 30) fails the build when a manifest disagrees.

| Kernel piece | Today | Stays because |
| --- | --- | --- |
| IR | `crates/ir`. No I/O. | Every codec and every plugin speaks these types. |
| Codecs | `crates/codec`. `Codec::id` at `crates/codec/src/lib.rs` line 38. Ids in tree: `anthropic-messages`, `openai-chat`, `openai-responses`, `generative-language`, `bedrock-converse` (`crates/codec/src/converse.rs` line 36). | Protocol encode and decode are hub-and-spoke. A vendor adapter calls a codec. It does not own one. |
| Quirks engine | `crates/quirks/src/lib.rs`. `Profile` line 12, `Rule` line 23, `named` line 44, `apply` line 49. Profiles: `responses-default`, `messages-default`, `no-sampling`. | Rules remove or fill canonical fields. Gateway and router do not read profiles (crate header, lines 1-6). |
| Router | `crates/router`. Re-exports `Provider` from `hofund-provider` at `crates/router/src/lib.rs` line 13. Ordered failover. `Router::from_hops` (line 108) freezes membership. | Routing is product behavior shared by every upstream. |
| Gateway | `crates/gateway/src/lib.rs` lines 1-6. Authenticates, decodes, asks the router, writes the body, records usage (`record` at line 627). | The HTTP edge. Vendor names are forbidden here by `gateway_and_router_do_not_name_vendors` (`xtask/tests/workspace_invariants.rs` line 190), which scans only `crates/gateway` and `crates/router`. |
| Config loader | `crates/config/src/lib.rs`. `Config` line 16, `Provider` line 51, `load` line 281. Raw structs use `deny_unknown_fields` from line 667. Zero workspace dependencies (`expected_deps` returns `&[]`). | Settings stay a leaf. Preset ids stay strings. |
| Secrets | `crates/secrets/src/lib.rs`. `resolve` line 34 (`env:NAME` or a literal). `TokenFile` line 49. The value is a plain `String`. | `architecture.md` lines 119-120: the daemon resolves secrets and passes material down. `provider` must not read the store. |
| Usage ledger | `crates/usage/src/lib.rs`. SQLite `requests` table at line 58. `Ledger::record` line 80. | `gateway` records one row per request. `usage` does not depend on `ir`. |
| Daemon | `crates/daemon/src/lib.rs`. `providers` line 231 matches `planned.wire` and constructs `OpenAiCompat`, `Anthropic`, `Generative`, `Vertex::new` (line 261), and `Bedrock::new` (line 275). `plan` line 297 calls `resolve`. | Process wiring. After the seam it accepts a registry and stops naming those types. |
| CLI shell | `crates/cli/src/main.rs` `Command` line 56: `serve`, `config check`, token show and rotate, `usage`, `agent list`, `connect`, `disconnect`, `status`. | A shell. It does not own vendor behavior. |
| Menu-bar shell | `apps/desktop` (`hofund-desktop`). Depends on `daemon` and `agent`. It embeds the daemon (`apps/desktop/src-tauri/src/control.rs` lines 11-13). | A shell. After section 3 it is a client, not a composition root. |
| TUI shell | Not in the tree. [roadmap.md](../roadmap.md) lines 51-60 describe a future terminal UI as a thin client of the management API, with a design doc `docs/tui-design.md` that does not exist yet. | Another shell. It is not a plugin. |

### What is coupled today, and therefore leaves the kernel

`Provider` (`crates/provider/src/lib.rs` lines 60-101) has `id`, `count_input` (default `CountOutcome::Unsupported`, line 73), and `execute` (line 96). The same crate also contains every vendor adapter (`anthropic`, `azure`, `bedrock`, `generative`, `mantle`, `openai_compat`, `sigv4`, `vertex`), the HTTP transport (`http.rs`), and the 58-preset catalog (`presets.rs`, `presets.toml`).

`Credential` (`crates/provider/src/http.rs` lines 24-44) includes `GoogApiKey`, documented as the `x-goog-api-key` header the Generative Language API requires. That is a vendor rule living next to the shared transport. `Endpoint` (lines 46-62) stores `api_key: String` and derives `Debug`, so a debug print shows the key. `Client::builder` runs in every `Upstream::open` (line 128) and again for Vertex tokens (`vertex.rs` line 172).

`Wire` (`crates/provider/src/presets.rs` lines 18-32) mixes protocol selection with vendor URL shapes. `resolve` (line 103) special-cases `google-vertex` (line 106) and returns an error for `remote-gateway` (line 109). The replacement is `ProtocolId` in the contract below, stated once.

`hofund_config::Provider` carries vendor fields: `region` (line 59), Vertex `project`, `location`, `credentials`, `impersonate` (lines 77-89), and Bedrock `access_key_id`, `secret_access_key`, `session_token` (lines 90-101). `Kind` (line 105) names `Anthropic`, `Generative`, and `Converse`. `deny_unknown_fields` means a new plugin setting is a kernel edit.

The daemon match at `crates/daemon/src/lib.rs` lines 251-285 is the composition root, and it names `Vertex` and `Bedrock`. That is why a provider change and a kernel change collide.

`AgentAdapter` (`crates/agent/src/lib.rs` lines 224-316) is the right shape for the engine: detect, read, plan, apply, revert, disconnect (drift: unchanged keys restored, user-edited keys kept, `force` restores the backup; the method is lines 271-286), `refresh_token`, `configured_model`, `status`. At the original RFC pin, the four adapters were Rust constants in `crates/agent/src/catalog.rs`; F1-F4 moved Codex, Claude Code, Gemini CLI, and OpenCode to the compiled registry. The old const engine and migration-only oracle feature have since been removed, and compiled descriptions use the plugin API slots directly. `FileKind` (`crates/agent/src/edit.rs` line 9) is `Json`, `Toml`, `Env`. `GatewayTarget` (line 67) is `base_url`, `token`, and `models` (line 77). There is no subscription trait anywhere in the workspace. Vertex ADC token mint (`crates/provider/src/vertex.rs`) is API auth for feature-parity row 2.4, which [subscriptions.md](../coverage/subscriptions.md) line 13 already separates from sign-in.

Streaming is already pull-based (`crates/provider/src/lib.rs` lines 5-7, `stream.rs` lines 3-4). That stays. What is missing is the host, the factory, and failure isolation.

### Stable plugin API and helper SDK

Two crates. `hofund-plugin-api` is the contract and stays small. `hofund-plugin-sdk` holds helpers that can change without a trait change. A plugin may depend on the API, and on the SDK when it uses a helper. It may depend on nothing else in the workspace.

On Rust 1.99 a trait that uses native `async fn` is not dyn-compatible. The contract uses `#[async_trait]` and `Arc<dyn ...>`. One boxed future per call is negligible next to network I/O. A later MSRV may switch the same traits to `dynosaur` or `trait-variant`. That switch is a new API version, not a silent edit.

`hofund-plugin-api` owns:

- the traits in section 2, including `HostContext`'s handles
- `PluginError` (section 1, Errors). `ProviderError` is folded into it. PR B may keep `type ProviderError = PluginError` until callers move, and PR C deletes the alias
- `Completion` and `CountOutcome`, which today sit in `crates/provider/src/lib.rs` (lines 43-56). `Completion` gains `Raw` (section 1, Passthrough)
- `ProtocolId`, the five codec protocols, and `CredentialStyle`: `Bearer`, `ApiKey`, `XApiKey`, `MessagesBoth`, or `NamedHeader`. The current `Credential::GoogApiKey` arm becomes `NamedHeader("x-goog-api-key")` supplied by the generative plugin. The header bytes on the wire stay the same
- the newtypes and the agent-description types the traits take
- named re-exports of `hofund_ir::Request`, `hofund_ir::Response`, and the stream event type. Not a glob. PR B marks those IR structs `#[non_exhaustive]`
- `const API: &str = "1"` for the future process host's handshake. It has no compile-time consumer in stage 1

`hofund-plugin-sdk` depends on `hofund-plugin-api` and owns:

- generic HTTP transports for the five protocols, moved out of `crates/provider/src/http.rs`, `openai_compat.rs` (the non-Mantle path), `anthropic.rs`, and the generative call path. They call `HostContext::http()`. They do not build a `reqwest` client. Azure listing and any other plugin call this transport. They do not depend on an OpenAI plugin
- the SigV4 helper that today is `crates/provider/src/sigv4.rs`. Bedrock is the caller. The kernel does not sign
- OAuth helpers shared by subscription plugins: PKCE, loopback callback, and device-flow polling. Vendor URLs stay in the plugin. The helper binds `127.0.0.1` only, checks `state`, uses a one-shot timeout, fails cleanly when the port is busy (Codex uses fixed port 1455), and does not log codes
- the process spawner used by `HostContext::spawn_process`: absolute path, no shell, an explicit environment, and a hofund-owned config directory
- retry and paging helpers used by `list_models` and `probe`
- a re-export of `tracing`. A plugin that needs a span uses that re-export. It does not depend on the `tracing` crate

`quirks::apply` is re-exported by the SDK so a plugin can pass a profile id. Adding a `Rule` variant stays a kernel change. The quirks crate continues to carry no vendor name (`crates/quirks/src/lib.rs` lines 41-43). The API does not depend on quirks.

`Request` stays defined in `hofund-ir`. The router depends on `hofund-plugin-api` only for `PluginError`, `Request`, `Completion`, and `MemberId`. It does not name `UpstreamProvider`. The daemon adapts `Arc<dyn UpstreamProvider>` into `Member`. Config keeps preset ids and protocol ids as strings and does not learn the registry. Unknown ids fail when the factory is resolved, the same class of error `resolve` returns today.

Kernel paths, after adapters have moved, are covered by the vendor-name scan that today covers only gateway and router. The scan stays off those paths while `crates/provider` still contains the adapters. Section 6 says when it turns on. There is no allowlist. Inbound surfaces are plugins identified by `SurfaceId`, not by a kernel enum, so a new surface does not edit the API.

### Host context

Every `build` and every subscription call receives `&HostContext`. `HostContext` and its handles are concrete types in `hofund-plugin-api`. Each handle wraps an `Arc<dyn ...Backend>`. The backend traits, and the only public constructor of `HostContext`, live in a new kernel-only crate, `hofund-plugin-host`, which depends on the API. A plugin can call the handle methods. It cannot construct a handle, and it cannot implement a backend, because it cannot name that crate as a normal dependency. `xtask` rejects a normal dependency on `hofund-plugin-host` from `plugins/`. A plugin reaches the host crate only through its kit dev-dependency. Kits (`publish = false`, test-only, only ever plugin dev-dependencies) may depend on `hofund-plugin-host` and expose `test_host(...) -> HostContext` with fake backends. The word "sealed" is not used for these handles. Sealing (`mod sealed`) is only for a trait that `hofund-plugin-api` both declares and implements. This contract has no such trait.

- `http()` is the kernel-owned client. One pooled client exists per `(proxy, TLS)` profile, so members that share a host reuse connections and the OS trust store is loaded once per profile. The vocabulary is `http::Request<Body>` and `http::Response<BodyStream>`, plus `bytes::Bytes`. The client applies the proxy, TLS, and User-Agent profile, and the kernel's tracing layer. A plugin does not build a client, pick a timeout, or load roots.
- `secrets()` returns `secrecy::SecretString`, re-exported by the API. The value is zeroized on drop and its `Debug` is redacted. `hofund_secrets::resolve` stops returning `String`. The SDK transport sets `HeaderValue::set_sensitive(true)` when it inserts the header. A plugin must not clone the secret into a struct field that derives `Debug`, and must not log it.
- `accounts()` is the account store. Updates are atomic and take a cross-process lock, because the daemon and a CLI refresh can race (feature-parity row 11.3). `AccountStore` keeps metadata in a mode `0600` JSON file and secrets in the OS keychain or a mode `0600` XChaCha20-Poly1305 file. The fallback has a separate mode `0600` random key file and logs why native storage is unavailable. Opening the store migrates legacy plaintext rows under the exclusive lock and removes the legacy copy after the new metadata commits. This satisfies the storage prerequisite of pull request H. The store keeps an in-memory cache and drops it on every write, so a request does not touch the keychain (and does not raise a prompt) on each call. Plugins do not open the files.
- `clock()` is the kernel clock. Plugins do not call `SystemTime::now` for quota windows or deadlines. Tests pass a fixed clock.
- `scheduler()` runs tracked, cancellable periodic jobs (list refresh, warm-up, quota alerts). A plugin registers a job. It does not spawn an untracked task. Shutdown cancels the jobs.
- `spawn_process()` is the only way to run a binary. No shell. The SDK resolves an absolute path, sets the environment, and uses a hofund-owned config directory. `Bridge { command: "claude" }` goes through this handle.
- `cache_dir()` and `state_dir()` are per-plugin directories created by the kernel. A plugin does not invent a path under the user's home.
- `observe()` accepts a `QuotaObservation` (rate-limit headers on a successful response). Combined with `PluginError::retry_after()`, this is how the kernel learns that a member should cool down. The plugin does not edit router state.

`build` may clone handles. They are `Arc` clones of the handle, not of the secret. `execute` reads the current secret from `secrets()` or `accounts()` on each call, so a refresh is visible without rebuilding the member. That read hits the in-memory cache, not the keychain, until a write invalidates it.

In-process plugins run with the daemon's privileges. That is accepted for reviewed in-tree code. A capability table in `plugin.toml` is follow-up N5. The future process host enforces it. Stage 1 reviewers check the table once that follow-up lands. Until then, review checks the crate's dependency list.

`unsafe_code = "forbid"` stays in force. Every plugin manifest sets `[lints] workspace = true`. `xtask` rejects a plugin that does not.

### Contract types

Every public struct and enum in `hofund-plugin-api` is `#[non_exhaustive]`. Struct fields are private. Callers use a builder, a `TryFrom`, or an accessor. Invariants such as "non-empty", "kebab-case", and "ASCII" are checked in the constructor, not by a later string check.

Newtypes, each with `TryFrom<&str>` that rejects anything other than kebab-case ASCII:

- `PluginId`
- `PresetId`
- `ProtocolId`
- `AccountId`
- `MemberId`
- `SurfaceId`

`MemberId` is built from a preset or subscription id plus a key or account id. It is stable across router snapshots, so cooldown and usage survive a reload. It is not a formatted `String` that plugins concatenate.

`SurfaceId` names an `InboundSurface` plugin. Agent `requires` is `&'static [SurfaceId]`. `plugins/registry/build.rs` checks each id against the compiled surface set. A new surface is a plugin pull request. It is not a new enum variant in the API, and it is not an allowlisted vendor name.

`TierId` is a newtype for a non-empty ASCII lowercase tier name (`opus`, `sonnet`, `haiku`, `fable`, `fast`). `TryFrom` rejects empty strings and anything outside `[a-z0-9]`. It is not kebab-case, because those names are the tokens Claude already uses. The Fable skip rule is `when_model_matches` in TOML, not a special case inside `TierId`.

Every type that `plugins/registry/build.rs` places in a `static` has a `#[doc(hidden)] pub const fn` constructor, or a `const` builder. That set is `UpstreamSpec`, `AgentDescription`, the file and set entries, `Slot`, and `RouteSpec`. Generated code calls those constructors. It does not use a struct literal, and it does not use `Box::leak`. `const` evaluation is what makes build-time validation and zero startup parsing both true: private fields and `#[non_exhaustive]` would otherwise reject the generated code, because it lives outside the API crate.

`AgentIntegration`, `UpstreamFactory`, `UpstreamProvider`, `SubscriptionFactory`, `Subscription`, and `InboundSurface` are `Send + Sync + 'static`, so instances can live in `Arc` across tasks.

### Semver policy

Two versions, kept separate:

- In-tree Rust API. `#[non_exhaustive]`, private fields, and a `cargo-public-api` snapshot in CI are the compatibility tools. A pull request that changes the public surface of `hofund-plugin-api` includes the snapshot diff. A breaking change (removing a trait method, tightening a bound, or changing a documented default) updates every in-tree plugin in that same pull request. There is no caret range and no out-of-tree plugin in stage 1. `cargo-semver-checks` against the merge base is an acceptable substitute for the snapshot if it is wired on the API crate; the requirement is a reviewed diff of the surface, not a particular binary.
- Process-host handshake. `API = "1"` is the string a future host and a plugin manifest compare. It is not consulted at compile time. Caret ranges wait until a second handshake version exists. Magpie's lesson is `internal/pluginrt/pluginrt.go` lines 24-27 (`API = "1.0"`, caret ranges on `Manifest.API`).

### Errors

One error type. `PluginError` is a `thiserror` struct, `#[non_exhaustive]`, with private fields and accessors:

- `kind() -> PluginErrorKind`
- `status() -> Option<u16>`
- `retry_after() -> Option<Duration>`
- `source() -> Option<&(dyn Error + 'static)>`

`PluginErrorKind` is `#[non_exhaustive]`: `Connect`, `Timeout`, `Transport`, `Interrupted`, `Status`, `Decode`, `Unsupported`, `Auth`, `Quota`, `Config`, `Cancelled`, `Io`, `Internal`. `Interrupted` means the upstream answered with a 2xx status and then its body failed or stalled; it is retried on the same member only for an idempotent call. A non-2xx body-read failure preserves the original `Status` and `Retry-After`, using empty bytes when necessary. Headers an upstream nominates in `Connection` are hop-by-hop and are stripped, including `Retry-After`. Constructors include `PluginError::unsupported(message)` and `PluginError::interrupted(message)`. The router matches `kind()` with a wildcard arm, so a new kind does not break the router. Today `ProviderError` has public fields, no `source`, no `retry_after`, and no `#[non_exhaustive]` (`crates/provider/src/error.rs` lines 10-33), and the router matches `ProviderErrorKind` exhaustively (`crates/router/src/lib.rs` lines 39-43).

`CountOutcome::Unsupported` stays a successful "this endpoint has no count method". It is not `PluginErrorKind::Unsupported`. The error kind means the call was rejected. The outcome means the feature is absent and the caller should continue.

These types are specified in this document and implemented in PR B: `ListQuery`, `ProbeQuery`, `QuotaQuery`, `ModelList`, `ProbeReport`, `SignInCtx`, `SignInProof`, `SignInFlow`, `SignInChallenge`, `SubscriptionSpec`, `Account`, `QuotaWindow`, `QuotaObservation`, `Plan`, `GatewayTarget`, `DisconnectOutcome`, `ConnectionStatus`, `Schema`, `RawTable`, `ValidConfig`, `ConfigError`, `BuildError`, `WriteCtx`, `ImportCtx`, `ImportedProvider`, `ModelSpec`, `RouteSpec`, `SurfaceHost`, `Body`, and `BodyStream`. `Plan` moves out of `crates/agent/src/lib.rs` line 174. `GatewayTarget` moves out of line 67, and its token field is a `SecretString`. `AccountPool` is not a plugin type. The account store on `HostContext` replaces it. PR B implements `From<ConfigError> for BuildError`, because `validate` returns `ConfigError` and `validate_and_build` returns `BuildError`.

`ModelList::source_url` records the URL that answered a list request for host-side response deduplication. `with_source_url` strips userinfo and fragments and replaces the query with `<redacted>` before storing it, keeping the record's derived `Debug` output free of URL credentials and query values.

### Lifecycle, isolation, and reload

Order for every plugin instance: `validate`, then `build`, then `health` (default `Ok(())`), then later `shutdown(&self, deadline)` (default no-op). `health` and `shutdown` live on the instance, because that is what owns background work. `shutdown` is called for token refresh loops, list refresh, and loopback listeners. The deadline is the kernel's drain budget.

The kernel, not the plugin, wraps every plugin future:

- `tokio::time::timeout` with the kernel deadline. The per-endpoint timeout on `Endpoint` (`crates/provider/src/http.rs` line 61) moves to that kernel deadline. A plugin does not run its own request timer.
- `FutureExt::catch_unwind` around `AssertUnwindSafe`. A panic marks that member failed and leaves it out of the next snapshot. The gateway stays up. PR K, which merges after C, adds the timed cooldown on top of that failed state. C does not wait for K and does not start a timer K has not defined.

Plugin futures must be cancel-safe. Dropping the future cancels the upstream call, including closing the connection when the body is dropped mid-stream.

A stream error after the first event is encoded as the client protocol's error event. It is not a clean EOF. Today `sse_body` returns `None` on `Some(Err(_))` (`crates/gateway/src/lib.rs` lines 573-576; the review cites line 577, the next arm). PR C changes that arm.

The router holds an immutable snapshot behind `arc_swap::ArcSwap`. Config reload, sign-in, and sign-out build a new snapshot from the registry and store it. In-flight requests finish on the snapshot they started with. `MemberId` does not change across snapshots, so cooldown and usage state carry over. Membership is resolved at startup and on every membership change, not only at startup.

[architecture.md](../architecture.md) line 148 says the running process does not reload configuration. This proposal changes that for provider, subscription, and route membership. The bearer-token re-read on each request stays as it is. Hot reload of the snapshot is PR L's config application, using the `ArcSwap` type introduced in PR C. PR C lands the type and the startup snapshot. PR L publishes a new snapshot when the control plane writes config or a sign-in completes.

### Observability

The kernel owns spans and metrics. The workspace has no `tracing` dependency today (rows 12.2 and 12.5 are Missing). PR B adds the dependency. PR C wraps the seam.

The kernel wraps `execute`, `list_models`, `probe`, `quota`, `refresh`, and agent `apply` in spans. Fields: `plugin.id`, `preset.id`, `member.id`, `attempt`, `protocol.in`, `protocol.out`, `status`, `ttft_ms`. Metrics: requests, errors by kind, latency, time to first token, and tokens. Plugins may add child spans only through the SDK's `tracing` re-export. Redaction happens in the kernel layer, before a span field or a capture body is stored. A secret must not appear in a span.

Request capture with bodies (row 12.1) is the kernel capture layer in section 2.4, with masking applied first. It is not a plugin.

### Same-protocol passthrough

When `protocol.in` equals `protocol.out`, the gateway forwards `bytes::Bytes` and only observes them. `Completion` gains:

```rust
/// Proposed. Not compiled by this pull request.
Raw(BodyStream<Bytes>),
```

A usage tap still records token counts when the bytes expose them, and records "unknown" when they do not. The common case is Claude Code to the Messages API and Codex to the Responses API. Decode-then-encode remains the path when the protocols differ, and the path that applies quirks.

`model_request.clone()` before dispatch (`crates/gateway/src/lib.rs` line 303) goes away. Usage is recorded from a small metadata struct (ids, token counts, member, timing). The per-attempt `Request` clone in the router (`crates/router/src/lib.rs` lines 154 and 206) stays acceptable until a later change to `Arc<Request>` with a copy-on-write model field. That later change is not part of this proposal.

`criterion` benches for decode, encode, and stream relay live in the workspace and run in CI. A comparison against magpie is a goal: an operator records the baseline and the feature-parity note cites it. Magpie is not a crate dependency, and CI does not fetch it.

### Kernel `Member`

`hofund-router` defines `Member`. The daemon implements it for an adapter that holds `Arc<dyn UpstreamProvider>`, the `MemberId`, and the failed-or-cooled state. `Member` stays an `#[async_trait]` trait so the router does not depend on `tower`. The daemon adapts each `Member` into a `tower::Service`, and the layer chain wraps that service. Router evolution does not change the plugin contract.

```rust
/// Proposed. Not compiled by this pull request.
/// Kernel-internal. Not in hofund-plugin-api.
#[async_trait]
pub trait Member: Send + Sync {
    fn id(&self) -> &MemberId;
    async fn execute(&self, request: Request) -> Result<Completion, PluginError>;
}
```

`PluginError` in that signature is the one shared type. `Member::id` returns `&MemberId`, so the router crate depends on `hofund-plugin-api` for four types: `PluginError`, `Request`, `Completion`, and `MemberId`. It does not depend on `UpstreamProvider` or `HostContext`. If that edge is still too wide, PR C moves those four types to a tinier `hofund-kernel-types` crate that both the router and the plugin API depend on. `xtask` rejects a router use of `UpstreamProvider` or `HostContext`. The inversion that matters is that the router does not name the plugin trait.

## 2. Plugin kinds and contracts

Four kinds: upstream, subscription, coding-agent integration, and inbound surface. Anything else in magpie's `plugins/` tree is placed in section 2.4. It is not a fifth kind by default.

A plugin is data when the behavior is a file write or a URL table. It is a crate when the behavior is a credential flow, a signature, a retry, or a write the slot language cannot express.

The kernel calls an upstream through a layer chain. The chain is Tower `Layer`s around the `tower::Service` the daemon built from `Member`. Stage 1 layers are timeout, panic containment, and tracing. The cooldown gate joins in PR K. Later layers (masking, budgets, capture, tool repair) are kernel pull requests that append to that chain. They are not plugins. This is the mechanism that keeps policy out of `gateway` and `router` source.

### 2.1 Upstream

Covers one implementation, which may register many preset ids. The catalog is 58 `[[preset]]` tables in one file (`crates/provider/presets.toml`, loaded by `include_str!` at `crates/provider/src/presets.rs` line 14). `resolve` special-cases `google-vertex`, `remote-gateway`, and `azure` by id (lines 106, 109, and 128). Every other id selects a URL by wire. The plugin unit is that shared implementation, not one crate per preset id. Data-only presets are TOML fragments compiled into static data by `plugins/registry/build.rs`. Code plugins in stage 1 are the ones the daemon already special-cases:

| Implementation | Why it is code | Where the behavior lives now |
| --- | --- | --- |
| `openai-compat` | Generic Chat and Responses transport, shared by the URL-only presets. | `crates/provider/src/openai_compat.rs` `OpenAiCompat::new` |
| `anthropic` | Messages transport. | `crates/provider/src/anthropic.rs` |
| `generative` | `generateContent` plus the `x-goog-api-key` placement. | `crates/provider/src/generative.rs`, `Credential::GoogApiKey` |
| `azure` | Deployment listing. The key is `api-key`. | `list_deployments` at `crates/provider/src/azure.rs` line 53 |
| `vertex` | OAuth and URL shape. The body is the generative codec. | `crates/provider/src/vertex.rs`, `models_for` line 195 (static per-location catalog) |
| `bedrock` | SigV4 and the Converse path. The codec stays in the kernel (`bedrock-converse`). | `crates/provider/src/bedrock.rs`, `crates/provider/src/sigv4.rs` |
| `mantle` | Prefix retry from the model id. | `crates/provider/src/openai_compat.rs` lines 3-5 and `crates/provider/src/mantle.rs` |

`count_input` exists and defaults to unsupported. `list_models`, `probe`, and `quota` do not exist on main. Feature-parity rows 2.9 and 2.10 are Missing. Azure listing is the only live list, and it is a function rather than a trait method. Pull request #28 stays unmerged until these methods exist on the trait.

Static metadata lives on the factory, not on the instance. There is no `UpstreamProvider::spec`.

```rust
/// Proposed. Not compiled by this pull request.
/// `ValidConfig: Send + Sync + 'static`, defined in PR B. It does not need `Any`.
/// The associated type is what the plugin validates. The registry stores
/// `Box<dyn DynUpstreamFactory>`. The blanket
/// `impl<F: UpstreamFactory> DynUpstreamFactory for F` calls `validate` and
/// then `build` on the typed value, so there is no downcast.
/// `validate` returns `ConfigError`. `validate_and_build` returns `BuildError`.
/// PR B implements `From<ConfigError> for BuildError`.
pub trait UpstreamFactory: Send + Sync + 'static {
    type Config: ValidConfig;

    fn spec(&self) -> &'static UpstreamSpec;
    fn config_schema(&self) -> Schema;
    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError>;
    fn preflight(&self, cfg: &Self::Config) -> Result<(), ConfigError> {
        let _ = cfg;
        Ok(())
    }
    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError>;
    fn build_with(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        if Some(ctx.protocol()) == self.spec().protocols().first().copied() {
            return self.build(cfg, host);
        }
        Err(BuildError::new("protocol requires a build_with override"))
    }
}

pub trait DynUpstreamFactory: Send + Sync + 'static {
    fn spec(&self) -> &'static UpstreamSpec;
    fn config_schema(&self) -> Schema;
    fn validate(&self, raw: &RawTable) -> Result<(), ConfigError>;
    fn preflight(&self, raw: &RawTable) -> Result<(), ConfigError>;
    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError>;
    fn validate_and_build_with(
        &self,
        raw: &RawTable,
        host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError>;
}
```

`BuildContext` is a non-exhaustive typed struct whose `protocol()` accessor returns the selected `ProtocolId`. Its public constructor also lets plugins and kits construct build tests. The default `build_with` supports only the spec's first protocol, so a multi-protocol plugin must opt in rather than silently using the wrong wire; `validate_and_build` retains that first-protocol behavior. The erased entry point rejects a protocol absent from `spec().protocols()` before calling the plugin. A typed context is preferable to a reserved raw-table key: it does not weaken config typing, mix kernel data with values validated twice, or require every listed schema to accept a new key.

```rust
#[async_trait]
pub trait UpstreamProvider: Send + Sync + 'static {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError>;

    async fn count_input(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<CountOutcome, PluginError> {
        let _ = (request, inbound);
        Ok(CountOutcome::Unsupported)
    }

    async fn list_models(&self, query: &ListQuery) -> Result<ModelList, PluginError> {
        let _ = query;
        Err(PluginError::unsupported("this upstream has no model list"))
    }

    /// One minimal request per speakable protocol.
    /// `ProbeQuery::minimal_request` and `ProbeReport::from_completion`
    /// are defined in PR B. `from_completion` reads a unary body, or the
    /// first stream event, and stops after a small cap. It does not drain
    /// an unbounded stream. `hofund provider test` calls this.
    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self.execute(query.minimal_request(), query.protocol()).await?;
        Ok(ProbeReport::from_completion(completion))
    }

    async fn quota(&self, query: &QuotaQuery) -> Result<Vec<QuotaWindow>, PluginError> {
        let _ = query;
        Err(PluginError::unsupported("this upstream has no quota endpoint"))
    }

    async fn health(&self) -> Result<(), PluginError> {
        Ok(())
    }

    async fn shutdown(&self, deadline: Duration) -> Result<(), PluginError> {
        let _ = deadline;
        Ok(())
    }
}
```

`UpstreamSpec` (defined in PR B, fields private) carries the plugin id, the protocols it speaks, the credential placement, and `ListKind` (`Live`, `Catalog`, `Deployments`, `Unsupported`), with explicit capability rank `Unsupported < Catalog < Deployments < Live`. The [models plan](../plugins/models.md) specifies the rank and Azure behavior. A declarative capability set for the UI is follow-up N2. Until N2, `ListKind` is the only capability bit, and the others are discovered by the documented defaults above.

`config_schema()` returns a JSON Schema (`schemars` in the API, or a small schema type defined in PR B if `schemars` is too heavy; PR B picks one and records it). The desktop renders it without a vendor match. `validate` runs before `build`. Invalid options never construct a provider.

### Config shape

`hofund-config` keeps kernel keys only, and it keeps zero workspace dependencies (rule 7). Those keys are `id`, `preset`, `base_url`, `protocol` (a `String`), `api_key` (still an `env:NAME` spec, resolved by the kernel into a `SecretString`), `headers`, `quirks`, `max_output_tokens`, the kernel request deadline, and the `models` table (`[providers.models]`, with `refresh`, `auto_include`, and `url`). The [models plan](../plugins/models.md) specifies these host-owned listing settings. Plus an opaque `options` table (`[providers.options]`). `base_url` and `protocol` stay because they select a codec and an endpoint. They are not vendor settings. The daemon converts `protocol` with `ProtocolId::try_from` when it resolves the factory. An unknown id fails there, the same class of error as an unknown preset id. `Kind` (`OpenAiCompat`, `Anthropic`, `Generative`, `Converse`, from line 105) is deleted. The string that replaces it is validated at that boundary. Config does not name `ProtocolId`.

`[subscriptions]` is the same shape for accounts: an id, which accounts to serve, and `[subscriptions.options]` for the vendor host and client ids. PR B2 adds both option tables. The selector stays a kernel key. The plugin validates `options`.

These fields move into `options`: `project`, `location`, `credentials`, `impersonate`, `access_key_id`, `secret_access_key`, `session_token`, and `region` (`crates/config/src/lib.rs` lines 59 and 77-101). The plugin validates them. The kernel does not name them. PR B2 keeps the old keys working for one release: they load, they are copied into `options`, and the load warns. The next release rejects the old keys.

A URL-only preset is a fragment, not a crate. The shape already in `presets.toml` (the `anthropic` and `openai` tables at lines 29-47) moves with the implementation that serves it. `plugins/registry/build.rs` compiles every fragment into static preset data. Startup does not parse TOML. Validation fails the build, not the process. A builtin factory without a plugin crate uses the data-only location `plugins/providers/<builtin-factory-id>/presets.toml`, with no `Cargo.toml` or `plugin.toml`.

Builtin-owned rows are served per wire by `openai-compat`, `anthropic`, and `generative`; the selected wire chooses the factory as it does for a catalog row. Each row lives in its default wire's factory directory: the first non-empty preset URL in `generative`, `chat`, `responses`, `anthropic` order determines that wire. Generative uses `generative`, Chat and Responses use `openai-compat`, Messages uses `anthropic`, and a row without wire URLs uses `openai-compat`.

```toml
# plugins/providers/openai-compat/presets.toml
[[preset]]
id = "openai"
name = "OpenAI"
kind = "vendor"
chat = "https://api.openai.com/v1"
responses = "https://api.openai.com/v1"
```

```toml
# plugins/providers/azure/plugin.toml
id = "azure"
kind = "upstream"
crate = "hofund-plugin-azure"
presets = ["presets.toml"]
```

There is no `credential` key in v1: builtin fragments derive credential placement from the preset, wire, and region-resolved host exactly as the catalog does, avoiding a second source of truth.

### 2.2 Subscription

No trait and no sign-in code exist. The contract source is [subscriptions.md](../coverage/subscriptions.md): 33 rows, all missing (lines 9-13). The P1 built-ins (lines 60-61) are Codex (ChatGPT via the Codex backend), the Claude Code bridge, Copilot including GitHub Enterprise Cloud with data residency, and Gemini Code Assist.

Magpie runs those from the provider package. `internal/provider/signin.go` lines 3-9 and 37-42: Codex OAuth at `https://auth.openai.com/oauth/authorize`, loopback `127.0.0.1:1455`, tokens in `logins.json`. Claude is signed in by Claude Code (`claude_signin.go`), which the coverage row states as `claude auth login` rather than a direct OAuth client. Copilot is a device flow. Gemini Code Assist is loopback OAuth. Quota windows are named per row (Codex rate-limit headers, Copilot entitlement, Claude ratelimit headers, Gemini `retrieveUserQuota`).

Stage 1 implements those four as native Rust plugins, one directory each. Copilot on a GitHub Enterprise host is an `options` field of the Copilot plugin, validated by `SubscriptionFactory::validate`, not a second crate. The static `Device` URL in the sketch cannot express that host. Follow-up N9 changes the enum before the Copilot pull request. The 13 community plugins that magpie no longer serves from core (coverage lines 16 and 31-42) wait for the process host in section 3.

`SubscriptionFactory` is the same shape as `UpstreamFactory`, including `type Config` and the erased `DynSubscriptionFactory` blanket. It is not `#[async_trait]`, because `build` is synchronous. The instance does not own the account list. The daemon asks `HostContext::accounts()` for the accounts of this `PluginId`.

```rust
/// Proposed. Not compiled by this pull request.
pub trait SubscriptionFactory: Send + Sync + 'static {
    type Config: ValidConfig;

    fn spec(&self) -> &'static SubscriptionSpec;
    fn config_schema(&self) -> Schema;
    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError>;
    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError>;
}
```

```rust
/// Proposed. Not compiled by this pull request.
#[async_trait]
pub trait Subscription: Send + Sync + 'static {
    fn id(&self) -> &PluginId;

    /// Declarative description for the UI. The HTTP steps stay in the plugin
    /// and use HostContext. `SignInFlow` is defined in PR B.
    fn flow(&self) -> &SignInFlow;

    async fn start_sign_in(
        &self,
        ctx: &SignInCtx,
        host: &HostContext,
    ) -> Result<SignInChallenge, PluginError>;

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError>;

    /// Writes the rotated secret through `host.accounts()`. Does not return
    /// a snapshot for the caller to cache.
    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError>;

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError>;

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError>;

    /// Builds the hop for this account. Reads the current secret from `host`
    /// on each later `execute`. Infallible capture of an `Account` value is
    /// not the contract.
    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError>;

    async fn health(&self) -> Result<(), PluginError> {
        Ok(())
    }

    async fn shutdown(&self, deadline: Duration) -> Result<(), PluginError> {
        let _ = deadline;
        Ok(())
    }
}
```

`SignInChallenge` is declarative so a UI can render it without a vendor match. The variants, defined fully in PR B, are at least: open a URL, show a device code, or show instructions for a bridge. `SignInFlow` variants are `Pkce`, `Device`, `TokenFile`, and `Bridge`. `Bridge` names a binary the spawner resolves. It is not a shell string.

`complete_sign_in` stores the account through `host.accounts()` and returns the `AccountId`. Codex returns a ChatGPT-backend `UpstreamProvider`. Copilot returns the Copilot API client. Gemini Code Assist returns the Code Assist client. Claude returns the bridge client. The router never sees `Subscription`. It sees a `Member`.

#### How an account becomes a router member

Feature-parity row 2.12 models keys as router members. Row 3.7 does the same for accounts. The subscription plugin does not insert itself into the router.

1. Config names a subscription id and which accounts to serve (one id, or every account in the store for that plugin). The selector is kernel config. The vendor host and client ids sit in `options`.
2. At startup, and again on every membership change (sign-in, sign-out, config reload), the daemon loads the factory, calls `upstream` once per selected account, and adapts each result to a `Member`.
3. The `MemberId` is `MemberId::from_parts(subscription, account)`. It is stable across snapshots.

Ordered failover and PR K's cooldown exist today; [architecture.md](../architecture.md) describes the member cooldown book. Cooldown is a kernel mechanism and merges before any subscription plugin. A plugin pull request does not edit `crates/router`.

Row 3.8 is the shared quota model: windows, resets, and credits. Two feeds, both kernel-owned:

- Polled: `Subscription::quota` fills `QuotaWindow`. The scheduler refreshes it. An exhausted window cools that member down.
- Passive: `PluginError::retry_after()` and `HostContext::observe(QuotaObservation)` report rate-limit headers seen on a call, including a successful response. PR K applies the header cap to error hints and rests a successful observation only when its window is exhausted.

The plugin returns observations. It does not call the router, and it does not import `hofund-router`.

### 2.3 Agent integration

`Described` in `crates/agent/src/catalog.rs` already implements `AgentAdapter` from a const `Description`. Connect, disconnect, and drift are engine behavior (`disconnect` at `crates/agent/src/lib.rs` lines 271-286). A plugin that only names paths and keys supplies data. The engine performs every write, takes the `*.hofund.bak` backup, and restores user-edited keys.

The engine stays in `hofund-agent`, which depends on `hofund-plugin-api`. The API does not depend on the engine and does not call `engine::plan`. That call would be a cycle, or it would drag the engine into the API.

`FileKind` has no YAML. Goose and Hermes need YAML ([agents.md](../coverage/agents.md) describes their files). That is one kernel change, `FileKind::Yaml`, not a crate per agent. Per-OS paths are data: `Session` already reads `HOME` or `USERPROFILE`, `PATH`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GEMINI_CLI_HOME`, and `XDG_CONFIG_HOME` (`from_process_env`, lines 108-111).

On this pin the four consts already write more than a URL and a token:

- Gemini CLI writes `model.name` (at the original pin: `catalog.rs` lines 176-178 and 246, `Slot::GeminiModel` at line 369). A string `model` is moved there. The sets now live in `plugins/agents/gemini-cli/agent.toml`, and the old private slot has been removed. [agents.md](../coverage/agents.md) line 18 records the same fact.
- OpenCode writes one `models` entry per route, with `name` and `variants` set to `{}` (at the original pin: `catalog.rs` lines 267-270, `Slot::OpenCodeModels` at line 370). Modalities, limits, and reasoning are omitted because this build has no catalog values. The catalog-model set now lives in `plugins/agents/opencode/agent.toml`, and the old private slot has been removed.
- Claude Code writes `env.ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, and `ANTHROPIC_DEFAULT_FABLE_MODEL` only for a Fable id (at the original pin: `catalog.rs` lines 31-37 and 72-94, Claude's tier slot at line 375). The tier sets now live in `plugins/agents/claude-code/agent.toml`, and the old private slot has been removed. `CLAUDE_CODE_SUBAGENT_MODEL`, `effortLevel`, and `CLAUDE_CODE_EFFORT_LEVEL` stay unset.

A crate exists only when a slot cannot express the write. Known cases from the coverage inventory:

- an OS credential store (the Zed row)
- an OS environment store (cursor-local writes `launchctl`, `HKCU`, or `environment.d`; there is no file)
- a sealed file (Hanako's AES-GCM catalog)

Those crates implement `custom_write`. Everything else is data. OpenCode's models object is `Slot::CatalogModels`, filled from the kernel model spec. Claude's tier env vars are `Slot::ModelTier`. The Fable rule moves into Claude's `agent.toml` as `when_model_matches = "claude-fable-"`. It is not a variant in the kernel enum.

`plugins/registry/build.rs` compiles `agent.toml` and `presets.toml` into `'static` values. Validation runs at build time. The process does not parse those files at startup and does not leak strings to make them `'static`. During migration, an agent that has no compiled description yet still uses the Rust const in `hofund-agent`. The pull request that adds the toml deletes the const after the golden plan matches. There is no runtime "if the file exists" branch.

#### Data, kernel commands, and code

| Need | Row | Kind | Where it lives |
| --- | --- | --- | --- |
| Selected model key | 5.5, `magpie claude opus` | Data | `model` on `AgentDescription`. `hofund agent set <agent> model=<id>` is a daemon command. It checks the id against the catalog and then calls the engine. |
| Effort key | 5.5 | Data | `effort` on `AgentDescription`. Claude's effort keys are optional data, not a Claude crate. |
| Visible model list | 5.7 | Data | `visible` on `AgentDescription`: all routes, or an allow-list. |
| Profiles | 5.6 | Kernel | A TOML snapshot of every agent's files. One control-plane call saves and restores the set. |
| Model catalog in the agent file | 2.8, 2.14 | Data, one engine slot | `Slot::CatalogModels`. OpenCode is the first caller. Empty `variants` remains the fallback when the spec has no reasoning levels (`catalog.rs` lines 268-270). The catalog service is section 2.4. A plugin does not fetch models.dev. |
| Picker-only agents | [agents.md](../coverage/agents.md) line 102 | Data | `cursor`, `copilot`, and `devin` set `route = "none"` and write only the model id. No base URL. |
| Import providers from another app | 2.17, 2.18 | Optional hook plus a kernel URL handler | `import_providers` on `AgentIntegration`. The `hofund://import` handler is kernel. |
| Inbound surfaces an agent needs | [agents.md](../coverage/agents.md) lines 104-109 | `InboundSurface` plugins | `requires` is `&'static [SurfaceId]`, checked by `build.rs`. The agent plugin does not register an HTTP route. |

```rust
/// Proposed. Not compiled by this pull request.
pub trait AgentIntegration: Send + Sync + 'static {
    fn description(&self) -> &'static AgentDescription;

    /// `Some` when this agent writes a credential store, an OS environment,
    /// or a sealed file. The default is `None`, and the engine uses slots.
    /// `GatewayTarget` and `Plan` are types in this crate. `hofund-agent`
    /// depends on the API and does not define them.
    fn custom_write(
        &self,
        ctx: &WriteCtx,
        target: &GatewayTarget,
    ) -> Result<Option<Plan>, PluginError> {
        let _ = (ctx, target);
        Ok(None)
    }

    /// Rows 2.17 and 2.18. The default is an empty list.
    fn import_providers(&self, ctx: &ImportCtx) -> Result<Vec<ImportedProvider>, PluginError> {
        let _ = ctx;
        Ok(Vec::new())
    }
}

#[non_exhaustive]
pub enum Slot {
    Origin,
    V1,
    Token,
    Literal(&'static str),
    /// Tier env var. `when_model_matches` skips the write unless the model
    /// id contains that substring. Claude's Fable rule lives in agent.toml.
    ModelTier {
        tier: TierId,
        when_model_matches: Option<&'static str>,
    },
    /// One models object per visible route, filled from the kernel model spec.
    CatalogModels,
}
```

`AgentFactory` is the same associated-type shape, erased as `DynAgentFactory`. A data-only agent has no crate. The registry synthesizes a factory whose schema is empty and whose `build` returns the compiled description. `AgentDescription` fields (private, `const` constructor in PR B) are: id, display name, binary, files, sets, optional model key, optional effort key, `visible`, `route` (`Gateway` or `None`), and `requires: &'static [SurfaceId]`.

```rust
/// Proposed. Not compiled by this pull request.
pub trait AgentFactory: Send + Sync + 'static {
    type Config: ValidConfig;

    fn description(&self) -> &'static AgentDescription;
    fn config_schema(&self) -> Schema;
    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError>;
    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn AgentIntegration>, BuildError>;
}

/// Erased factory stored by the registry. The blanket
/// `impl<F: AgentFactory> DynAgentFactory for F` calls `validate` and then
/// `build` on the typed value. `BuildError: From<ConfigError>`.
pub trait DynAgentFactory: Send + Sync + 'static {
    fn description(&self) -> &'static AgentDescription;
    fn config_schema(&self) -> Schema;
    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn AgentIntegration>, BuildError>;
}
```

The engine in `hofund-agent` performs plan, apply, disconnect, status, and `refresh_token` by reading the description and, when `custom_write` returns `Some`, applying that plan with the same backup and drift rules.

The Codex const (`catalog.rs` from line 102) becomes:

```toml
# plugins/agents/codex/agent.toml
id = "codex"
display_name = "Codex CLI"
binary = "codex"

[[file]]
id = "config"
kind = "toml"
linux = "~/.codex/config.toml"
macos = "~/.codex/config.toml"
windows = "~/.codex/config.toml"
env_override = "CODEX_HOME"

[[set]]
file = "config"
key = "model_provider"
slot = { literal = "hofund" }

[[set]]
file = "config"
key = "model_providers.hofund.base_url"
slot = "v1"

[[set]]
file = "config"
key = "model_providers.hofund.experimental_bearer_token"
slot = "token"

[model]
file = "config"
key = "model"
```

The first migration of this agent must produce a byte-identical plan against the Rust const, with a golden file per OS. New agents are TOML only.

### 2.4 Where magpie's other features live

Each row is one of: plugin kind, kernel layer, kernel service, or a later RFC. This is the placement. It is not a promise to build them in stage 1.

#### InboundSurface

The plugin does not bind a socket and does not depend on `axum` or `hyper` (rule 9). It declares paths and handles a request the gateway has already accepted. `RouteSpec` is the HTTP method plus a path prefix. `SurfaceHost` is a concrete type in the API, constructed only in `hofund-plugin-host`. It exposes the current router snapshot, the codecs, and the auth result for this request.

```rust
/// Proposed. Not compiled by this pull request.
#[async_trait]
pub trait InboundSurface: Send + Sync + 'static {
    fn id(&self) -> &SurfaceId;
    fn routes(&self) -> &'static [RouteSpec];
    async fn handle(
        &self,
        route: &RouteSpec,
        req: http::Request<Body>,
        host: &SurfaceHost,
    ) -> Result<http::Response<BodyStream>, PluginError>;
    async fn health(&self) -> Result<(), PluginError> {
        Ok(())
    }
    async fn shutdown(&self, deadline: Duration) -> Result<(), PluginError> {
        let _ = deadline;
        Ok(())
    }
}
```

PR D mounts these routes in the gateway. `plugins/registry/build.rs` fails the build when two `RouteSpec` prefixes conflict. On a request, the gateway matches the prefix, authenticates, then calls `handle` inside the kernel span and the kernel timeout. PR S depends on that mount. It does not add a listener of its own.

| Capability | Placement | What that means |
| --- | --- | --- |
| Inbound surfaces: Ollama `/api/version`, `/api/tags`, `/api/show`, `/api/chat`; `/muse-code/models`; Codex model catalog; `/backend-api/codex`; embeddings; audio (rows 1.7-1.10, [agents.md](../coverage/agents.md) lines 106-109) | Plugin kind `InboundSurface` | One directory each under `plugins/inbound/<id>`. The trait above declares `routes` and `handle`. The gateway mounts them in PR D. The codec stays in the kernel. `crates/gateway` does not name these vendors. Agents require a `SurfaceId`, checked by `build.rs`. |
| Gateway middleware `onRequest`, `onEvent`, `onResponse` (rows 8.1-8.3) | Kernel layer chain | Tower layers. A later RFC may host JS or WASM middleware as a layer. Stage 1 does not. |
| Routing strategies, stickiness, cooldown, `Retry-After`, RPM, and concurrency limits (rows 4.5-4.7, 4.14) | Kernel service | A strategy trait plus member state on `MemberId`. Ordered failover stays the only strategy until that trait grows. Cooldown is PR K, before H. Not a plugin. |
| Rate-limit and quota headers on a response (row 3.8) | Kernel service | `PluginError::retry_after` and `HostContext::observe`. Polled `quota()` remains for a vendor that publishes a quota URL. |
| Account discovery from an agent's own sign-in (row 3.6) and account import (row 3.7) | Kernel service `AccountSource` | Plugins cannot depend on each other. The service reads agent files through the agent engine and writes `HostContext::accounts()`. |
| Import providers from other apps (row 2.17) and import links (row 2.18) | Optional `AgentIntegration::import_providers`, plus a kernel URL handler | The handler accepts the link and calls the hook. No plugin imports another plugin. |
| models.dev catalog, names, prices, limits, reasoning (rows 2.8, 2.14) | Kernel catalog service | Fetches, caches with an ETag, and ships an offline snapshot so a build works without the network. Plugins and the agent engine read `ModelSpec`. They do not fetch. |
| Warm-ups, check-ins, list refresh, quota alerts (rows 3.10-3.12, 2.9) | Kernel scheduler | `HostContext::scheduler`. Plugins register jobs. They do not spawn tasks. The 10 minute and 15 minute numbers from magpie are chosen in the plugin pull request that needs them, not in this document. |
| Outbound proxy, insecure TLS per provider, User-Agent (rows 10.10, 2.21, 2.11) | Kernel HTTP profiles | `HostContext::http()` as specified in section 1. A plugin selects a profile id. |
| Vendor JSON-schema fixes for DeepSeek, Kimi, and Grok (row 1.17) | Upstream plugin hook, or a quirk profile shipped inside that plugin | Not kernel code. `crates/quirks` stays free of vendor names. The plugin calls `quirks::apply` with its profile id. |
| Tool-call repair (row 1.19) | Kernel layer | Protocol repair on the layer chain. |
| Prompt cache on the Messages protocol (row 1.15) | Kernel layer, keyed by `ProtocolId` for `anthropic-messages` | Not a vendor branch in `crates/gateway`. |
| Compaction on the Responses protocol (row 1.16) | Kernel layer, keyed by `ProtocolId` for `openai-responses` | Same rule. |
| Privacy masking (rows 13.x), request capture (row 12.1), budgets and gateway-key limits (rows 9.12, 10.4) | Kernel layers | Masking runs before capture. Budgets and key limits are layers on the same chain. Stating this keeps them out of plugin crates. |
| Built-in plugin on/off, Requires/Provides, UI `Parts` (`pluginrt.go` lines 29-60) | Not a runtime unlink in stage 1 | The registry links every in-tree plugin. Per-member enable in config is follow-up N8. UI forms are `config_schema()` and `SignInChallenge`, not a `Parts` tree. |
| Live settings: `/api/settings`, `/api/agents/*`, `/api/profile/*` | Control plane, section 3 | The hofund paths are `/v1/hofund/*`. Same job: settings, agents, and profiles without restarting the UI. |
| Library: instructions, MCP servers, skills, hooks (rows 6.x); web search and image MCP (rows 7.x) | Later RFC | A stage-2 `LibraryWriter` may reuse the agent engine's slot writes. Not a kind in this proposal. |
| Backup, sync, self-update, autostart, awake, statusline, WSL (rows 14, 16.2-16.4, 15.8-15.9, 5.9-5.10) | Later RFC, shell and ops | Socket handoff and the background-service model are part of the control plane and land with PR L. The rest wait. |

## 3. Registration, loading, and the control plane

### Compile time

Each plugin that has code is one crate. A feature on the registry crate selects it. Two mechanisms can collect the crates:

| Mechanism | What it gives | What it costs on this repo |
| --- | --- | --- |
| Explicit registry | `plugins/registry/build.rs` reads every `plugin.toml`, emits a `match` from id to factory, compiles preset and agent TOML into static data, and the registry `Cargo.toml` lists one feature and one dependency per implementation. `xtask` fails the build when the list is unsorted, missing, has a duplicate `(kind, id)`, or reuses a package or feature name. | One shared manifest. Two pull requests that append different lines conflict in one hunk. Resolution is to keep both lines and re-run the check. |
| `linkme` or `inventory` | Each crate registers itself through a distributed slice. No central list. | The workspace already treats an explicit member list as the review surface (`MEMBERS` at `xtask/tests/workspace_invariants.rs` line 11). A missing registration on one target is a silent omission. CI runs the test job on Windows (`desktop` and `test` matrix). Distributed slices depend on the platform linker. `cargo test -p <plugin>` passes when the plugin's tests pass even if the binary never linked it. |

### Runtime

Magpie's community OpenCode auth plugins are JavaScript, run on Bun. The coverage file says so (line 16) and points at `github.com/magpie-community/plugins`, which that inventory did not review. The host in tree at the pin is `internal/plugin/host.go`: it embeds `host.js` (line 33) and `pi.js` (line 40), speaks JSON lines (`message` at line 43), and imports `os/exec` (line 18). `Loaded.Provides` (line 499) are the provider ids an auth hook signs in to. `Clash` (lines 509-517) records two plugins that claim one id. The host runs one of them.

A WASM host does not load those JavaScript modules. This workspace forbids `unsafe` (`AGENTS.md`). Streaming HTTP and OS credential stores need a capability host either way. Stage 1 does not build that host.

### Stage 1 recommendation

Compile-time explicit registry, generated from `plugin.toml`.

The set of plugins is visible in review, which matches how `MEMBERS` and `expected_deps` already work. A duplicate preset id fails in `xtask` on every OS, including the Windows test job. `cargo test -p hofund-plugin-azure` proves that plugin. The registry crate is the only package whose manifest grows, and it grows by sorted append-only lines.

Feature flags live on `hofund-plugin-registry`, one per implementation crate, default features naming all of them so the daemon links every in-tree plugin. Features are additive. A developer builds a subset with `--no-default-features --features azure`. Data-only presets and data-only agents have no feature, because they have no crate. The registry's generated code calls the SDK transport for those ids.

`plugins/registry` is the only crate allowed to depend on plugin implementation crates (packages under `plugins/<kind>/`). The composition root in the end state is the `hofundd` binary in `apps/hofundd` (package `hofund-daemon-bin`). After L it is the only package that links the registry directly. After L, `hofund-daemon` the library takes the registry as an argument and does not name plugin crates, so a plugin pull request does not edit the daemon manifest. The CLI does not depend on the registry directly. Until L it links it transitively through `hofund-daemon`, and rule 3's ban (including transitive links via `cargo metadata`) turns on in L. Desktop and TUI do not depend on the registry directly. Until L the desktop links it the same way, through the embedded daemon.

Today the only binary is `hofund`, from `crates/cli/Cargo.toml` lines 14-16, and `hofund serve` is `Command::Serve` at `crates/cli/src/main.rs` line 29. That call runs the daemon in-process. In the end state, `hofund serve` in `apps/cli` execs `hofundd` with the same config path and does not link the registry, directly or transitively. `hofundd` is the process that listens.

Until PR L, provider commands, subscription commands, and `hofund serve` may still run in-process through the daemon library, which is what the current binary does. Pull request #28 and batch H use that path. They do not wait for L. L is what switches those commands onto the control client and what makes `hofund serve` the exec. The desktop rework that drops the embedded daemon is the same pull request's follower, and it waits on L.

Today the desktop embeds the daemon, and `providers` (`crates/daemon/src/lib.rs` line 231) is the composition root. From D until L the library still calls `registry.factory`, so the desktop's embed is a temporary edge. L moves that link onto `hofund-daemon-bin` and deletes it. PR L updates the architecture table in the same change.

The process host for the 13 community subscriptions is a later RFC. It would be a program outside the kernel speaking a line protocol, following `internal/plugin/host.go`, and it would register through the same `Subscription` trait. Coverage priority P3 already says to choose a host or a native port before implementing that set. The handshake version is `API`.

### Control plane

One daemon process serves both the model gateway and the management API. Desktop, TUI, and the remote CLI commands are clients.

- Transport: loopback HTTP, plus a Unix socket or a Windows named pipe. Authenticated by the same token file the gateway uses.
- DTOs live in `hofund-control-proto`. Breaking that crate is a version bump of the local API (`/v1/hofund/*`).
- An event stream (SSE or WebSocket) carries usage, quota, routing decisions, and sign-in progress. The tray, the window, the TUI, and a future web UI render from it. They do not poll and they do not restart the gateway to see a new account.
- `config_schema()` and `SignInChallenge` are the only vendor-shaped data a client needs. The UI builds a form from the schema. It does not match on `google-vertex`.
- `hofund-desktop` and the future TUI depend only on `hofund-control-client` and `hofund-control-proto`. They do not depend on `daemon`, `registry`, or a plugin. A UI crash does not drop an in-flight model request. Socket handoff on update (row 16.3) is this boundary: the new UI connects to the running daemon.
- The CLI binary moves to `apps/cli` and stays named `hofund`. Commands that only touch local files (config check, a dry-run agent plan) may link `hofund-config` and `hofund-agent`. After L, commands that talk to a provider, a subscription, or the running gateway go through the control client, and `hofund serve` execs `hofundd`. Until L those commands may run in-process through the daemon library. The CLI does not depend on the registry directly. Until L it links it transitively through `hofund-daemon`, and rule 3's ban (including transitive links via `cargo metadata`) turns on in L.

PR L adds the proto, the client, the loopback listener, and the event stream (rows 12.3 and 11.2). The desktop rework that drops the in-process daemon is a later pull request that depends on L. This proposal does not do that rework.

## 4. Target layout, names, and dependency rules

### Layout

End state, after the mechanical moves in section 6. The first pull requests add `crates/plugin-api` and `crates/plugin-sdk` beside the current crates so the tree does not move in one change.

```text
crates/kernel/ir
crates/kernel/codec
crates/kernel/quirks
crates/kernel/router
crates/kernel/gateway
crates/kernel/config
crates/kernel/secrets
crates/kernel/usage
crates/kernel/daemon
crates/kernel/plugin-api
crates/kernel/plugin-sdk
crates/kernel/plugin-host          # backends and the only HostContext constructor
crates/control-proto
crates/control-client
apps/hofundd                       # package hofund-daemon-bin, binary hofundd
apps/cli                           # binary hofund; `hofund serve` execs hofundd
apps/desktop
apps/tui
plugins/providers/<id>          # plugin.toml, presets.toml, src/ when the plugin has code
plugins/subscriptions/<id>
plugins/agents/<id>             # agent.toml, src/ only when a slot is not enough
plugins/inbound/<id>
plugins/registry                # build.rs + the append-only manifest
kits/upstream                   # package hofund-kit-upstream, publish = false
kits/subscription
kits/agent
fuzz/                           # nightly targets, not a required CI job
xtask
```

Until that move, paths stay `crates/<name>` and the new crates are `crates/plugin-api`, `crates/plugin-sdk`, and `crates/plugin-host`. Plugins may land under `plugins/` before the kernel directory exists. `xtask` keys off the package name and the path prefix, not off the kernel directory. The CLI leaves `crates/cli` for `apps/cli` in PR J. `apps/hofundd` is added in PR L. The CLI is a shell, so it is not under `crates/kernel`. The daemon binary is not a shell and is not a kernel library either: it is the composition root, so it lives under `apps/`.

### Names

- Plugin id: kebab-case ASCII, equal to the coverage inventory id when one exists (`codex`, `claude-code`, `google-vertex`, `remote-gateway`). The type is `PluginId`. Ids are unique within each kind (`provider`, `subscription`, `agent`, and `inbound`), not across kinds, so the Codex agent and Codex subscription both keep the user-facing id `codex`. Config keys and CLI arguments remain bare, unqualified ids within their existing kind-specific commands and tables. The ids `api`, `sdk`, `host`, and `registry` are reserved in every kind and are not plugin ids. The crates `hofund-plugin-api`, `hofund-plugin-sdk`, `hofund-plugin-host`, and `hofund-plugin-registry` are infrastructure.
- Host storage uses `PluginKey`, the `(PluginKind, PluginId)` pair. Secrets, accounts, cache directories, and state directories are isolated by that pair; directories end in `<kind>/<id>`. Existing account rows without a kind remain subscription rows. `MemberId` is unchanged: its account variant already means a subscription, and its display form does not gain a kind prefix.
- Crate and package name: `hofund-plugin-<id>` when globally unique, or `hofund-plugin-<kind>-<id>` when needed to distinguish equal ids in different kinds. Registry feature names follow the same global-uniqueness rule. Existing package and feature names do not change. These are Cargo build identifiers, not user-facing plugin ids. Package path: `plugins/<kind>/<id>`. `plugins.toml` is sorted by id and then kind.
- Kit package name: `hofund-kit-<kind>`, `publish = false`.
- One implementation crate may own many preset ids. The fragment lists them. The id of the crate is the implementation id (`openai-compat`), and each `[[preset]]` id is the catalog id (`openai`, `deepseek`, and the other URL-only rows that share it).
- `remote-gateway` stays a preset id that `resolve` rejects (line 109). It does not become a kernel type.
- Protocol ids stay the codec ids, as `ProtocolId`, validated by the daemon from the config string. Vendor names do not appear in kernel paths. After the scan widens, they do not appear in kernel source. `SurfaceId` is a newtype, not a list of vendor names.

### Rules `xtask` enforces when the first plugin lands

PR B adds the checker next to `dependency_graph_matches_architecture`. It runs every rule that already has a subject: the host-dependency allowlist, the router name ban, `from_backends`, the API and SDK lint attributes, the `tracing` re-export, and kit `publish`. The `plugins/` walk runs when that directory exists. A fixture test rejects a bad tree, so the rules are not waiting on the first plugin to be written. PR D is when the first real plugin turns that walk on in the repository.

1. A package under `plugins/<kind>/` (`providers`, `subscriptions`, `agents`, `inbound`) may name `hofund-plugin-api` and, when it uses a helper, `hofund-plugin-sdk`. No other workspace dependency. Pure third-party crates (`ring`, `serde`, and similar) are allowed. `hofund-gateway`, `hofund-daemon`, `hofund-router`, `hofund-provider`, `hofund-ir`, `hofund-plugin-host`, `hofund-plugin-registry`, and any implementation crate `hofund-plugin-<id>` or `hofund-plugin-<kind>-<id>` are rejected. `plugins/registry` is rule 3, not this rule. The reserved ids in the Names section are not plugin ids.
2. No plugin depends on another plugin. This is the rule `TestPluginsImportOnlyCore` enforces for Go imports (`internal/pluginrt/pluginrt_test.go` line 94, described at `pluginrt.go` lines 8-10). Here it is a Cargo dependency check plus a walk of `use` paths.
3. The rule is by path, not by the name prefix `hofund-plugin-*`. Packages under `plugins/<kind>/` depend only on the API and the SDK (rule 1). `plugins/registry` is the only package under `plugins/` that may depend on implementation crates, meaning `hofund-plugin-<id>` or `hofund-plugin-<kind>-<id>` for a real plugin id. The ids `api`, `sdk`, `host`, and `registry` are reserved, so `hofund-plugin-api`, `hofund-plugin-sdk`, `hofund-plugin-host`, and `hofund-plugin-registry` are infrastructure. `hofund-router` depending on `hofund-plugin-api`, and `hofund-daemon-bin` depending on `hofund-plugin-registry`, are legal. The registry's dependency list and its feature list are sorted and equal to the `plugin.toml` files on disk. After L, `hofund-daemon-bin` (`apps/hofundd`) is the only package that depends on the registry directly. The CLI does not depend on the registry directly. Until L it links it transitively through `hofund-daemon`, and this ban (including transitive links via `cargo metadata`) turns on in L. The desktop does the same until L. `hofund-control-client` never depends on the registry. From D until L the library's registry edge is temporary and L removes it.
4. Preset ids are unique across fragments. The 58-id assertion that the catalog tests encode today moves to this check. Aliases stay aliases (the four `[[alias]]` tables at `presets.toml` lines 10-27).
5. The vendor-name scan grows from `crates/gateway` and `crates/router` to every kernel crate after the adapters have left `crates/provider`. There is no allowlist. Turning the scan on earlier fails the build on the adapters that are still there on purpose.
6. A plugin's dev-dependencies may include the kit for its own kind and ordinary test crates. A provider plugin does not depend on the agent kit. A runtime dependency on any `hofund-kit-*` is rejected.
7. `hofund-config` keeps zero workspace dependencies. Preset ids and `protocol` stay strings. The daemon validates `protocol` with `ProtocolId::try_from`. Unknown preset ids and unknown protocol ids fail when the factory is resolved, as `resolve` fails today.
8. No runtime crate depends on `xtask`.
9. Plugins may not depend on `reqwest`, `hyper`, `axum`, `tracing`, or on `tokio` features that include `net` or `process`. The `use` walk rejects `std::process`, `tokio::process`, `tokio::net`, `reqwest`, `hyper`, and `axum` anywhere under `plugins/`. HTTP and process spawn go through the SDK.
10. Every plugin manifest contains `[lints] workspace = true`.
11. Every code plugin has `tests/conformance.rs` calling `hofund_kit_<kind>::conformance`. A data-only plugin is covered by the registry fragment test instead.
12. `hofund-router` does not name `UpstreamProvider`, `Subscription`, or `HostContext`.
13. No package under `plugins/` depends on `hofund-plugin-host` as a normal dependency. A plugin reaches it only through its kit dev-dependency. Kits (`publish = false`, test-only) may depend on `hofund-plugin-host` and expose `test_host(...) -> HostContext` with fake backends.
14. Registry ids are unique per `(kind, id)`, with reserved ids rejected in every kind. Package names and feature names remain globally unique so Cargo packages, generated Rust identifiers, and feature gates cannot collide. Equal ids in different kinds are valid when those build identifiers are distinct.

`docs/architecture.md`, the crate manifest, and `expected_deps` still change together for every new edge. The edges this design adds, when the migration pull requests land, are:

- `hofund-plugin-sdk` depends on `hofund-plugin-api`
- `hofund-plugin-host` depends on `hofund-plugin-api`. Nothing under `plugins/` depends on `hofund-plugin-host` as a normal dependency. Kits may depend on it and expose `test_host(...) -> HostContext`
- `hofund-plugin-api` depends on `hofund-ir` and re-exports three types by name
- `hofund-router` depends on `hofund-plugin-api` only for `PluginError`, `Request`, `Completion`, and `MemberId`, or on `hofund-kernel-types` if PR C splits those four out. It does not depend on the provider trait
- `hofund-daemon-bin` (`apps/hofundd`) depends on `hofund-plugin-registry` and on `hofund-plugin-host`
- `hofund-control-client` depends on `hofund-control-proto`
- `apps/desktop` and `apps/tui` depend on `hofund-control-client` only
- `apps/cli` depends on `hofund-control-client` after L, and may depend on `hofund-config` and `hofund-agent` for offline commands. Until L it may depend on `hofund-daemon` for the in-process provider and subscription commands. It does not depend on the registry directly. Until L it links it transitively through `hofund-daemon`, and rule 3's ban (including transitive links via `cargo metadata`) turns on in L
- plugin packages depend on `hofund-plugin-api` and may depend on `hofund-plugin-sdk`

### Lints and feature checks

`check-plugins` is the mechanical gate for plugin crates. It is not a sandbox. The threat model, what the gate enforces, and the known limits are in [the plugin boundary note](../plugins/boundary.md).

Workspace lints stay `missing_docs = "warn"` and `clippy::pedantic = "allow"`. `hofund-plugin-api` and `hofund-plugin-sdk` additionally set `#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]`, `clippy::pedantic` at warn, and `missing_errors_doc` plus `must_use_candidate` on.

CI on the API crate stores a `cargo-public-api` snapshot (or `cargo-semver-checks` against the merge base). Surface growth is a reviewed diff.

CI runs `cargo hack check -p hofund-plugin-registry --each-feature --no-dev-deps` once the registry exists, so a missing `#[cfg]` fails. Features must be additive.

## 5. Conformance kits and real-tool acceptance

Each plugin pull request is self-verifying. The kit is shared so the plugin author does not copy the harness. The kit depends on the plugin API, the SDK, and test tools. It does not depend on a plugin. Calling the kit is mandatory.

The entry points are `hofund_kit_upstream::conformance(factory, fixture)`, `hofund_kit_subscription::conformance(...)`, and `hofund_kit_agent::conformance(...)`. `xtask` fails when a code plugin has no `tests/conformance.rs` that calls the entry point for its kind. That is the same idea as an OpenTelemetry `componenttest`: one suite, every implementation.

| Kind | Kit | Proves | Grown from |
| --- | --- | --- | --- |
| Upstream | `kits/upstream` | `execute` against wiremock, auth header placement, error class (`Connect`, `Timeout`, `Status`, `Decode`), `list_models` pagination when `ListKind::Live`, `probe` sends one minimal request and records a status, `quota` either returns windows or `PluginErrorKind::Unsupported`. Also: drop mid-stream closes the upstream connection, a kernel deadline fires, a mid-stream error becomes a protocol error event, `Debug` of any secret-bearing value does not contain the key, cancel-safety, and N parallel `execute` calls on one instance. | `crates/provider/tests/upstream.rs` (header: "Upstream adapters against a local mock. No real network.") and `crates/provider/tests/azure_deployments.rs`. |
| Subscription | `kits/subscription` | Recorded HTTP fixtures for `start_sign_in`, `complete_sign_in`, `refresh`, and `quota`. The account store sees an atomic update. No browser, no loopback to a vendor, no live OAuth in CI. | No hofund test exists yet. The fixture format is request and response files checked into the plugin directory. |
| Agent | `kits/agent` | Temp `Session`, golden bytes before and after `plan` for each OS, `apply` backup, `disconnect` drift (user-edited keys kept, untouched keys restored), `refresh_token` without a new backup. | `plugins/registry/tests/agent_conformance.rs` and `plugins/registry/tests/agent_regressions.rs`. |

A plugin pull request runs `cargo test -p <crate>` for a code plugin, or the registry's fragment test for a data-only plugin, plus `cargo test -p xtask --test workspace_invariants`. The fragment test checks the compiled static ids, required keys, and duplicate detection. It does not re-parse TOML at runtime.

Golden files live next to the plugin: `plugins/agents/codex/fixtures/<os>/`. Subscription recordings live at `plugins/subscriptions/codex/fixtures/`.

### Fuzzing

`fuzz/` holds `cargo-fuzz` targets for the SSE splitter, the AWS eventstream decoder (`crates/codec/src/eventstream.rs`), and `decode_request`. They run on nightly and are not a required CI job. Codec `proptest` round-trips stay (`crates/codec/Cargo.toml` lines 23-25). LAN clients are untrusted (row 10.5), so these parsers are the ones that get targets.

### How this meets the evidence rule

[feature-parity.md](../feature-parity.md) status key: a row is Done only with the code location, the tests, and the CI run that executed them. Behavior that a real agent or a real upstream has not exercised says `verified with mocks only` or `unverified`. A plugin pull request that lands mocked tests marks the coverage cell Partial and writes `verified with mocks only` plus the CI run URL. It does not mark the row Done.

### Real-tool acceptance

Required CI has no vendor secrets and does not log in to a browser. Acceptance is optional and sits outside the plugin:

- `kits/accept/<id>.md` names the command (`hofund provider test <id>`, or the subscription sign-in command once it exists) and the observation that counts as success.
- A `workflow_dispatch` job may run one id when an operator supplies secrets. It is not a required check.
- The feature-parity row cites that run's URL when it has one. Otherwise the row says `unverified`.

`crates/daemon/tests/http.rs` stays the kernel test that the gateway still serves a routed request after the registry is wired. Plugin authors do not edit it unless the route shape changes.

## 6. Migration and parallel work

No step below starts in this pull request. PR B starts only after this RFC is merged. This pull request does not add a plugin, a `CODEOWNERS` file, or a crate.

In flight, and not part of the proposal:

- Pull request #26 was squash-merged as `dfb6e89`. This pin already writes Gemini `model.name`, OpenCode `models` entries, and the Claude tier env vars (section 2.3).
- Pull request #28 (`cursor/provider-test-lists-692a`) is open and unmerged. It sketches `list_models`, `diff_ids`, `refresh_due`, and `run_probe`, and wires them through the daemon and the CLI ahead of this API. It stays unmerged until `UpstreamProvider::list_models` and `probe` exist. After PR B it rebases onto those methods, `PluginError`, and `HostContext`. It does not add a plugin-owned HTTP client. It is not the trait, and it is not a plugin. Until L it may keep the in-process CLI path. It does not wait for L. After L, `hofund provider test` goes through the control client.

### Pull requests, in order

Each one is branched from latest `main`. Each one updates `docs/architecture.md` and `expected_deps` when it adds an edge.

| Id | Change | Parallel with |
| --- | --- | --- |
| B | Add `hofund-plugin-api`, `hofund-plugin-sdk` (no transports yet), and `hofund-plugin-host` (backend traits and the `HostContext` constructor). Traits, `PluginError`, newtypes, `#[non_exhaustive]`, `const fn` constructors, named IR re-exports, and `non_exhaustive` on those IR structs. Add `tracing`. API and SDK lints, and the public-API snapshot in CI. `hofund-provider` and `hofund-agent` re-export the traits. No behavior change. | Nothing. This is the seam. |
| B2 | Move `project`, `location`, `credentials`, `impersonate`, `access_key_id`, `secret_access_key`, `session_token`, and `region` into `[providers.options]`. Add `[subscriptions]` and `[subscriptions.options]`. Config keeps `protocol` as a `String`. Delete `Kind`. Old keys still parse for one release. | After B. May proceed beside C. Must merge before E and before H. |
| C | Kernel `Member`, adapted to `tower::Service`, and the `ArcSwap` snapshot. Daemon adapts `UpstreamProvider` to `Member`. Router matches `PluginErrorKind` with a wildcard. Kernel timeout and `catch_unwind`: a panic marks the member failed and drops it from the next snapshot. No cooldown timer yet. Stream errors become protocol error events (`gateway` lines 573-576). Tracing spans on the seam. Same-protocol `Completion::Raw`, drop `model_request.clone()` (gateway line 303), and `criterion` benches. | After B. Beside B2. Before D, because C and D both edit `providers`. |
| D | `plugins/registry` and `build.rs` static presets, agent descriptions, and inbound routes, including the path-conflict check. Gateway mount: auth, span, and timeout, then `InboundSurface::handle`. Move transports, SigV4, OAuth helpers, retry, and paging into the SDK, on the pooled client. `xtask` rules, kits, mandatory `conformance`, `cargo hack --each-feature`, and `fuzz/` targets. Daemon Chat, Responses, Messages, and Generative arms call `registry.factory`. Vertex and Bedrock stay in the match until E. | After C. The pooled client and the ban on `reqwest` land here. |
| K | Kernel cooldown and passive rate-limit. `retry_after` and `observe` cool a `MemberId` down. Circuit-breaker state survives snapshot swaps. | After C. Must merge before H. May proceed beside D and E. |
| L | Control plane: `hofund-control-proto`, `hofund-control-client`, `/v1/hofund/*`, loopback plus socket or named pipe, token auth, event stream. Add `apps/hofundd` (`hofund-daemon-bin`, binary `hofundd`) and move the registry link onto it. `hofund serve` execs `hofundd`. Drop the registry edge from the desktop and from the in-process CLI path. Hot reload publishes a new `ArcSwap` snapshot. | After D. Before the desktop rework. Beside E, F, G, and S. H and #28 do not wait for it. |
| E | The preset-fragment format and loader land first. Then one implementation per pull request: azure, vertex, bedrock, mantle, then the URL-only fragments that ride `openai-compat`, `anthropic`, and `generative`, including the six ids `resolve` still rejects. Fragment batches are data-only moves of complete `[[preset]]` and `[[alias]]` rows; other pull requests move that code and its rows. `hofund-provider` keeps a re-export until the last caller is gone. The plugin depends only on the API and the SDK, and uses `HostContext::http()`. | After D and B2. Disjoint directories. Shared file: the sorted registry manifest. |
| S | `InboundSurface` plugins: Ollama, Muse, Codex backend, embeddings, audio. Each implements the trait. None opens a socket. | After D's gateway mount. Codex subscription joined mode waits on the Codex surface. |
| F | Four pull requests, one per existing agent (`codex`, `claude-code`, `gemini-cli`, `opencode`). Each adds `agent.toml`, compiles it in `build.rs`, and deletes the Rust const after the per-OS golden plan matches, including `model.name`, `ModelTier`, and the OpenCode `models` object as they behave on this pin. | After D. Does not wait for E. |
| G | `FileKind::Yaml` in the kernel engine, with a kit test. Then the remaining P1 agents as TOML: pi, goose, zed, vscode, crush, hermes, kimi, qwen, droid, cline, grok. A crate is added only for `custom_write`, in its own pull request. | After D. Goose and hermes wait on the YAML pull request. Zed is code if the slot cannot write the OS credential store. |
| H | Subscription plugins, one directory each: `codex`, `claude`, `copilot`, `gemini`. The `codex` subscription deliberately shares its bare id with the `codex` agent; the `(subscription, codex)` and `(agent, codex)` identities are distinct, and existing agent config and `hofund agent connect codex` are unchanged. Each implements `upstream`. Accounts are router members (rows 2.12 and 3.7). Quota uses PR K. N9 merges before Copilot. Sign-in commands use the in-process CLI path until L. The landed kind-scoped `AccountStore` satisfies the encrypted-storage prerequisite with metadata references and native or encrypted-file secrets before any of these plugins persists an account. | After D, B2, K, and encrypted account storage. Codex also after S. Does not wait for L. The four directories do not overlap. |
| I | Delete `crates/provider/presets.toml` once every fragment is compiled in. The catalog test still asserts the 58 ids and the four aliases. Widen the vendor-name scan. No allowlist. | After E and S. |
| J | Move `crates/<name>` to `crates/kernel/<name>`, and `crates/cli` to `apps/cli`. Mechanical. | After I. Not beside feature work. |

### PR B notes

PR B landed the seam and kept gateway behavior the same. These are the places the code does not match a sentence above, and why.

- Backend traits live in `hofund_plugin_api::kernel`. That module, `HostContext::from_backends`, `SurfaceHost::from_parts`, and `AuthResult::new` are public only with the non-default `kernel` feature, which only `hofund-plugin-host` enables. A handle holds `Arc<dyn Backend>`, and another crate cannot fill a private field, so the traits cannot live only in `hofund-plugin-host`. The constructors plugins and the kernel call are `host_context`, `surface_host`, and `test_host`, plus kind-scoped `host_context_for` and `test_host_for`. The original constructors remain provider-scoped for compatibility. A context stores `PluginKey`; `SecretsHandle::get` passes that kind-qualified key, so neither another id nor the same id in another kind can read the plugin's secret. `xtask` parses Rust with `syn` and reads `cargo metadata`, including build and target-specific dependencies. It rejects `from_backends`, `hofund_plugin_api::kernel`, `SurfaceHost::from_parts`, and `AuthResult::new` outside the API, the host, and the kits. The scan follows a renamed import, a crate alias, `extern crate`, a glob that pulls `kernel` into scope, a type alias, an angle-bracket call, and a path inside a macro. Plugin manifests are parsed as TOML: a renamed dependency (`package = "..."`), a `[dependencies.name]` table, a multi-line `features` array, and `[features]` forwarding `hofund-plugin-api/kernel` all count. A workspace build still unifies features, so CI runs `cargo check -p` for each plugin package on its own. That command does not build `hofund-plugin-host`, and the `kernel` feature stays off.
- `Schema` is a small ordered form (`empty`, `string`, `boolean`, `integer`, `array`, `object`). The API does not depend on `schemars`.
- `ProbeReport::from_completion` is async. The stream it reads is async. The sketch in section 2.1 was sync.
- `ProviderError` is not an alias of `PluginError`. The router still matches `ProviderErrorKind` exhaustively. The alias, and the wildcard, are PR C.
- `hofund_secrets::resolve` still returns `String`. No plugin calls it yet, and changing the return type would edit every caller for no current plugin.
- PR B originally shipped `PlaintextFileAccountStore`. The H prerequisite has since landed as `AccountStore`: mode `0600` metadata, native credential entries or XChaCha20-Poly1305 fallback records, and retry-safe plaintext migration. It retains the advisory sibling lock (shared for metadata snapshots, exclusive for writes, migration, and key publication) and an `Arc` cache dropped on every write. Secret reads can reach the keychain without that lock. A read reloads when the mtime, length, or inode changes. On Windows the file id is not read: `MetadataExt::file_index` is still unstable, so the stamp is mtime and length. Secrets are `SecretString` in memory and redacted. The scheduler accepts `Job` (`Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>`) and does not spawn tasks. `SchedulerHandle` has no `shutdown`. `cancel` rejects a job id the context did not register. `StdProcess::kill` sends the signal and reaps the child on a background thread; it does not wait on the caller. The daemon does not call the store, the clock, or the spawner.
- `Request` and `Response` are `#[non_exhaustive]`. Callers use `new` and `with_*` setters. There is no positional `from_parts`, because adding a field would otherwise break every caller. `StreamEvent` is not `#[non_exhaustive]`. It is an enum and already has an unknown variant. Marking the enum `non_exhaustive` would force a wildcard in every codec match before a plugin exists.
- `MemberId` is built with `from_preset` and `from_parts`. It has no `TryFrom<&str>`. A member id is not a string a plugin concatenates.
- `AccountId` stays kebab-case, as the Names section says. A vendor account id that is not kebab-case has to be mapped by that plugin. Widening the alphabet is a later change if a real account id cannot be mapped.
- The public-API snapshot is `crates/plugin-api/public-api.txt`, produced by a `syn` walk of the source. It records function signatures, `#[non_exhaustive]`, and derives. `cargo-public-api` needs rustdoc JSON, which on the pinned stable toolchain is still an unstable rustdoc option, so CI does not depend on it. `static_assertions` checks `Send + Sync` on the handles, `PluginError`, and the erased traits. `kebab_id!` types are included by reading that macro. The diff of the text file is the review. CI also runs `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps`.
- `test_host` takes the `PluginId` and remains provider-scoped; `test_host_for` also takes `PluginKind`. `TestHost` is the same backends with accessors for seeding secrets and HTTP responses. Its account store uses an in-memory credential backend, never the real keychain. Production `AccountStore::open` returns `Result`. Account rows carry a kind, with missing kinds read as subscription for compatibility.
- `From<BuildError> for PluginError` maps a build failure to `PluginErrorKind::Config`. This document required `From<ConfigError> for BuildError`, which is also implemented.
- `hofund-plugin-host` has only `hofund-plugin-api` as an internal workspace dependency. The hidden kernel module re-exports the HTTP request and response types and `expose_secret`, so the host crate does not name `http`. The encrypted account backend directly uses `secrecy` for key ownership.
- `hofund-provider` re-exports `UpstreamFactory`, `UpstreamProvider`, `SubscriptionFactory`, `Subscription`, `InboundSurface`, and the erased factories. `hofund-agent` re-exports `AgentFactory`, `AgentIntegration`, `DynAgentFactory`, `GatewayTarget`, `Plan`, `FileChange`, `DisconnectOutcome`, and `ConnectionStatus`. The existing `Provider` and `AgentAdapter` traits stay. `GatewayTarget::token` is a `SecretString`. Callers use accessors. The bytes written into agent files are the same.

### PR C notes

PR C puts the router on `Member` and `PluginError`. These are the places the code does not match a sentence above, and why.

- `Member` also has `route_id`, `protocol`, and `count_input`. Usage rows keep the configured provider id. `MemberId`'s display form is `preset:{preset}:{key}`, which is not that id. `protocol` lets an adapter tell a same-protocol passthrough from a translation. `count_input` keeps token-count routes on the same member list. The plugin trait is still not named by the router.
- The daemon adapts today's `Arc<dyn Provider>`, not `Arc<dyn UpstreamProvider>`. The provider trait has not moved. `ProviderError` stays in the provider crate. The daemon maps it to `PluginError`. The alias was not added.
- The kernel idle deadline is the endpoint timeout (60 seconds by default). The body also has a total deadline of twice that idle. Reqwest's whole-call timeout is longer, so a stall that went through the daemon reports the kernel's timeout. A socket that does not go through that layer, including the provider tests, still ends on its own.
- Spans and metrics wrap `execute` and `count_input` (`member` span; `hofund_member_requests`, `hofund_member_errors`, `hofund_member_ttft_ms`, `hofund_member_ttfb_ms`, `hofund_member_latency_ms`, and `hofund_tokens`). `plugin.id` is the built-in implementation (`openai-compat`, `anthropic`, `generative`, `google-vertex`, `bedrock`) until a plugin crate is the member. `preset.id` is the configured preset, or empty. `hofund_member_ttft_ms` and `hofund_member_ttfb_ms` are the time to the first byte. `hofund_member_latency_ms` is the whole request, recorded when the gateway writes usage. The daemon model aggregator wraps `list_models` and `probe` in spans with provider id, wire, kind, and outcome, and emits `model_list` and `provider_test` tracing events. Secret values and URL queries are not span or event fields. `quota`, `refresh`, and agent `apply` are not on the daemon call path, so they are not wrapped.
- A same-protocol body, including Converse, is `Completion::Raw`. The gateway forwards the bytes. It asks the codec to read token usage across chunk boundaries and to rewrite the model field to the name the client sent. A leading in-band failure is a status error, so the router can fail over. A failure after the first event is that protocol's error frame. A partial frame is closed with a blank line first. The content type comes from the codec. Cross-protocol still decodes and strips extensions.
- `BodyStream::resume` puts a peeked raw chunk back so failover can look at the first chunk and a success still delivers it. The router re-exports `PluginError`, `Request`, `Completion`, and `MemberId`. It also imports `PluginErrorKind`, `CountOutcome`, `BodyStream`, and `QuotaObservation`. `xtask` rejects any other plugin-api import, and it rejects `UpstreamProvider` and `HostContext`.
- `criterion` benches for decode, encode, stream relay, and the raw passthrough tap live in `crates/codec/benches/relay.rs` and run in CI. Magpie is not a dependency.
- The `ArcSwap` snapshot is stored while the membership lock is held. A panic in the call or while polling the stream publishes a snapshot that omits that member. There is no cooldown timer. PR K adds it. Config hot reload is pull request L. Duplicate member ids fail startup.

Quirks stay in the kernel for the whole sequence. A new `Rule` variant is a kernel pull request. Vendor behavior stays in the plugin that calls `quirks::apply` through the SDK.

### PR D notes

PR D1 lands the registry seam and the pooled client. These are the places the code does not match row D, and why.

- The pooled `reqwest` client lives in `hofund-plugin-host` (`PooledHttp`), keyed by proxy, insecure TLS, and the host timeout limits. [`HttpBackend::send`](../../crates/plugin-api/src/kernel.rs) is unchanged: one handle is one profile, and the API does not grow a transport type. `hofund-plugin-sdk::Call` builds the request, places the credential, and calls `HostContext::http()`. It does not construct a client. Plugins do not depend on `reqwest`, `http`, or `async-trait`. The SDK re-exports `async_trait` and aliases the HTTP types.
- `HttpLimits` is the host timeout config. The default is a 10 second connect timeout and a 60 second idle read timeout. The read timeout resets after each successful read, so a stream that keeps sending is not cut off. There is no whole-call timeout. `Call::send_with_retry` makes at most three attempts and waits a full-jitter exponential backoff between them. Dropping the future cancels the wait and does not start another attempt. `Connect` (DNS, TCP, or TLS before the request is sent) is retried for every method. `Timeout` and `Transport` (a reset after the request was sent and before a status) and `Interrupted` (a body-read failure or timeout after a 2xx status) are retried for `GET`, `HEAD`, `OPTIONS`, `PUT`, and `DELETE`, and for a call marked [`Call::idempotent`](../../crates/plugin-sdk/src/transport.rs). `POST` and `PATCH` are not retried once the body may have been processed, unless that mark is set. `plugins/registry/build.rs` also watches the `plugins` directory, so a new unlisted plugin directory reruns the bijection check.
- Plugins register only through `contribute() -> Contribution`. `plugins/registry/build.rs` reads `plugins.toml`, sorted by id and then kind, and each `plugin.toml`. It fails on a duplicate `(kind, id)`, a reserved id, a duplicate package or feature name, a missing entry, an unsorted list, or a same-method prefix conflict, and calls `contribute` behind one feature per plugin. Equal ids in different kinds are accepted. The two plugins are `relay` (provider, preset id equal to the plugin id) and `echo` (inbound, `GET /hofund/echo`). The fixture package stays outside the registry.
- `plugins/registry` root is the composition crate. A nested package under that directory is still rejected by `check-plugins`.
- The daemon still builds `Arc<dyn Provider>` in `providers()`. It does not call `registry.upstream`. Vertex and Bedrock stay in that match. Chat, Responses, Messages, and Generative stay there too, until an adapter from `UpstreamProvider` exists.
- The gateway does not mount `InboundSurface` yet. `match_route` is the lookup the mount will call. Auth, the span, and the timeout are not wired.
- `SigV4`, OAuth, paging, the 58-preset catalog, kit `conformance`, `cargo hack --each-feature`, and `fuzz/` targets are not in this pull request.

### PR D2 notes

D2 follows the review answers on D1. Shared protocol helpers stay in the SDK and are added in E, when the first plugin that needs each one migrates. They are not copied into a plugin.

- The daemon's Chat, Responses, Messages, and Generative arms call `registry.factory` and adapt the `UpstreamProvider` back to `Provider`. Vertex and Bedrock stay on the old constructors. The four factories still run the `hofund-provider` adapters, so E replaces the factory body without adding a second routing path.
- `plugins/registry/build.rs` indexes `PresetId` to a plugin id plus static config and fails on a duplicate or a non-kebab id. The catalog of 58 presets is not in that index yet.
- The gateway fallback authenticates, then calls `InboundSurface::handle` inside an `inbound` span and a 60 second timeout. `echo` is mounted, including a path under its prefix. Codec routes stay on their own axum paths.
- The daemon config supplies `connect_timeout_ms` and `read_timeout_ms` to `HttpLimits`. Omitted keys use the 10 second connect and 60 second idle-read defaults. These are the socket budgets. The kernel idle deadline stays the endpoint timeout.

### PR D3 notes

D3 adds the compiled agent-description path and its equivalence evidence. At D3, the four production adapters still used their Rust consts; F1-F4 migrate them after this proof.

- Delivery follows rule 3: the CLI and desktop call `hofund_agent::catalog(hofund_daemon::agent_descriptions())`. Until L, the daemon re-exports the registry's sorted `agents()` list without depending on the agent engine. L moves delivery behind the control plane.
- The API adds `FileKind::Env`, `AgentFile` `create` and `env_replaces`, shared-id ordered candidates, `Slot::ModelIfUnset`, model and effort file builders, lowercase match needles, and documented tier and catalog-model semantics. Compiled path overrides and the named setters treat empty and relative environment values as unset, matching the reference. Path templates remain `~`-rooted, and lifting that restriction is still an open question.
- `plugins/registry/build.rs` validates and compiles data-only `plugins/agents/<id>/agent.toml` files into static descriptions. These directories need no `plugin.toml` or crate and are not listed in `plugins.toml`. The non-default `test-fixtures` feature builds the separate fixture list; registry dev-dependencies enable it for tests.
- `hofund-kit-agent` depends on `hofund-agent` and replays plan, apply, backup, refresh, and disconnect contracts without network access. Registry tests also dev-depend on the kit. Goldens live at `plugins/registry/tests/fixtures/agents/<id>/fixtures/<os>/<case>/` or `fixtures/all/<case>/`: shared cases run on all three OSes, OS cases run only on their own OS, and regeneration moves identical three-OS trees to `all/`. Compiled descriptions replay these goldens, and shipped-plugin regression tests cover the remaining edge cases.
- F1-F4 map the existing private slots as follows: `GeminiModel` becomes `model_if_unset = { from = "model" }`, Claude's model slot becomes `model_if_unset = {}`, and Claude's tier slot becomes `model_tier`, with `claude-fable-` as the needle only for Fable. `OpenCodeModels` becomes `catalog_models`, and OpenCode's JSONC/JSON choice becomes two candidates with JSONC `create = false` and JSON `create = true`.

### PR F1 notes

F1 migrates Codex to the compiled data-only path without changing its configuration bytes or lifecycle behavior.

- `plugins/agents/codex/agent.toml` carries the former const's paths, ordered sets, model key, and rationale. The production Codex const and resolver are removed. The other three agents remain const-backed in this pull request.
- The temporary pre-F1 oracle feature was enabled only by agent and registry dev-dependencies, never by normal production composition. It was removed after F4.
- Goldens live at `plugins/agents/codex/fixtures/all/<case>/`. Fresh, existing-user, stale-hofund, multiple-route, environment override, and configured-model cases pass through both the shipped compiled description and the frozen oracle for Linux, macOS, and Windows. Registry integration tests also compare metadata and the complete edit lifecycle.
- `BUILTIN_ORDER` retains `claude-code`, `codex`, `gemini-cli`, `opencode`. A compiled description fills its named slot; other compiled ids follow in input order, and collision checks are unchanged. The temporary reserved-id list was removed after all agents migrated.
- No parity fixes or behavior changes are included.

### PR F2 notes

F2 migrates Claude Code to the compiled data-only path without changing its configuration bytes or lifecycle behavior.

- `plugins/agents/claude-code/agent.toml` carries the former const's path, ordered origin, token, model, and tier sets, Fable needle, model key, and rationale. The production Claude Code const, described adapter, and resolver are removed; Gemini CLI and OpenCode remain const-backed.
- F1's temporary oracle feature was renamed and extended to the frozen Codex and Claude Code adapters. It remained dev-only and was removed after F4.
- Goldens live at `plugins/agents/claude-code/fixtures/all/<case>/`. Fresh, existing-user, stale-hofund, single-route, Fable, multiple-route, commented, environment override, and configured-model cases pass through both the shipped compiled description and the frozen oracle for Linux, macOS, and Windows. Registry integration tests also compare metadata, errors, and the complete edit lifecycle.
- `BUILTIN_ORDER` retains `claude-code`, `codex`, `gemini-cli`, `opencode`. The temporary reserved-id list and static lookup functions were removed after all agents migrated.
- No parity fixes or behavior changes are included.

### PR F3 notes

F3 migrates Gemini CLI to the compiled data-only path without changing its configuration bytes or lifecycle behavior.

- `plugins/agents/gemini-cli/agent.toml` carries the former const's paths, ordered sets, model key, and rationale. An absolute `GEMINI_CLI_HOME` remains the parent of `.gemini`; empty and relative values remain unset. The production Gemini CLI const, described adapter, and resolvers are removed.
- The temporary dev-only oracle feature exposed all four frozen adapters after F3 and F4. The old const `Slot` enum and that feature have since been removed.
- Goldens live at `plugins/agents/gemini-cli/fixtures/all/<case>/`. Fresh, one-file and two-file user content, stale-hofund, string-model, existing-model, route-count, commented, configured-model, and environment override cases pass through both the shipped compiled description and the frozen oracle for Linux, macOS, and Windows. Registry integration tests also compare metadata, errors, directory trees, and the complete edit lifecycle.
- `BUILTIN_ORDER` retains `claude-code`, `codex`, `gemini-cli`, `opencode`. The reserved-id list and static lookup functions were removed in the #147 and #180 cleanup.
- F4 (OpenCode) merged first, so F3 leaves zero const-backed agents. The const engine types and fallback have since been removed, and a missing compiled built-in remains a hard `AgentError` (#131).

### PR F4 notes

F4 migrates OpenCode to the compiled data-only path without changing its configuration bytes or lifecycle behavior.

- `plugins/agents/opencode/agent.toml` carries the former const's ordered JSONC and JSON candidates, XDG override, provider sets, catalog-model slot, model key, and rationale. The production OpenCode const, described adapter, and path resolvers are removed.
- The temporary dev-only oracle feature also exposed the frozen OpenCode adapter. All migrated private slots and the feature have since been removed.
- Goldens live at `plugins/agents/opencode/fixtures/all/<case>/`. Fresh, JSON, JSONC, both-files, stale-hofund, route-count, configured-model, and XDG cases pass through both the shipped compiled description and the frozen oracle for Linux, macOS, and Windows. Registry integration tests compare metadata, errors, bytes, directory trees, and the complete edit lifecycle.
- `BUILTIN_ORDER` retains `claude-code`, `codex`, `gemini-cli`, `opencode`. The temporary reserved-id list has been removed.
- No parity fixes or behavior changes are included.

### PR B2 notes

B2 is the config shape from section 2.1. It does not add a plugin.

- `protocol` is a string. The daemon converts it with `ProtocolId::try_from`. Config does not name that type. A string that is not a codec id and not a legacy short name still loads and fails when the factory is resolved. That error names the provider and lists the codec ids.
- For this release, `kind` and the short names `chat`, `responses`, `messages`, `generative`, and `converse` still load. They are rewritten to `openai-chat`, `openai-responses`, `anthropic-messages`, `generative-language`, or `bedrock-converse`, and the load records a warning that names the replacement. `kind = "openai_compat"` with `protocol = "responses"` stays Responses. A `kind` that disagrees with `protocol` is an error. The next release rejects `kind` and the short names.
- `project`, `location`, `credentials`, `impersonate`, `access_key_id`, `secret_access_key`, `session_token`, and `region` load from `[providers.options]`. The same keys at the top of a provider still parse for this release: a non-empty value is copied into `options` and the load records a warning. The next release rejects them. Config does not check which preset they belong to.
- A string that is empty or only whitespace is absent. That applies to those legacy keys and to string values in `[providers.options]` and `[subscriptions.options]`, including a string nested in a table. The key is omitted. There is no error and no warning. A blank string inside an array is kept. `0`, `false`, an empty array, and an empty table are values and are kept.
- `[providers.options]` and `[subscriptions.options]` hold a string, an integer, a boolean, an array, or a table. Config does not name `RawTable`. A value of the wrong type for a factory's schema is a check error that names the provider and the key. `Debug` shows option keys and header names, and redacts every option value, every header value, and `api_key`. The table is opaque and may hold secrets, so config has no allowlist of safe keys. The factory schema is that allowlist. See PR E1 notes.
- The daemon forwards `options` into the factory. It does not construct `Vertex` or `Bedrock` itself. See PR E1 notes.
- `[[subscriptions]]` has `id`, `accounts` (`*` or one account id), and `[subscriptions.options]`. The daemon stores the table and does not build members. That is H. An id that no plugin knows still loads until H.

### PR E1 notes

E1 is the first slice of row E. The threat model is [docs/plugins/providers.md](../plugins/providers.md). The adversarial cases in that note are tests in this pull request.

- `hofund-plugin-sdk` now has SigV4 (moved from `hofund-provider`), OAuth token parsing and form bodies, a JSON page cursor, and `Call::json`. PKCE, loopback, and device flow stay in H. OAuth token caching, refresh, and single-flight become an SDK helper in row H (subscription plugins); Vertex keeps its own cache until then. A paging loop with a page limit and repeated-cursor guard is deferred to PR E2 (model listing).
- Every wire, including Vertex and Bedrock, is built with `registry.factory`. The adapters still live in `hofund-provider`. The factories validate option schemas before any option `env:` reference is resolved. Unknown keys and wrong types name the provider and the key and do not include the value.
- After that validation, the daemon trims string options and resolves `env:`; resolution errors name the provider and key. It rejects a reserved kernel key inside `options`. `plan` still reads `project`, `location`, and `region` to build catalog URLs.
- Built-in adapters take the pooled `reqwest` client from a kernel-only host-handle method, so `HttpLimits` apply. `HttpHandle::send` was unchanged here; PR H0g-a later made it stream 2xx bodies. Moving the five protocol transports into the SDK and streaming through `HttpHandle::send` is targeted at the first row E PR that moves an adapter into its own plugin crate (E3, Vertex), which also removes the raw-client method.
- `google-vertex` and `bedrock` are not compiled presets. `resolve` still owns their URLs. Per-preset `/models` and the connectivity probe are the next pull request.
- Mantle parity is complete in `hofund-provider` (Option C). Moving Mantle into its own crate waits for Option B: SDK streaming transport, then the Vertex crate, then the Mantle crate.

### PR K notes

PR K lands the member cooldown book and passive observation path. These are the design choices and the places the code differs from the RFC text.

- `hofund_router::Cooldowns` is keyed by stable `MemberId`. The daemon membership table owns one `Arc<Cooldowns>` and injects it into every published `Router`, so breaker state survives `Snapshot::store` even when every member instance is replaced. The book owns the generic, rate-limit, quota, credit, observation, and panic policies. Ready members keep configured order and go first; only the soonest cooling member remains as a probe, and a lone member is always tried.
- The daemon's tower cooldown layer in `crates/daemon/src/membership.rs` records every call outcome. Unary success clears cooldown state. A count never records success and cools only on 429 and transport-class failures, matching magpie `gateway.go:1427-1461` at `5b4da5b` (`unsupportedCount` breaks without resting at 1444-1446, 429 rests at 1447-1452, and transport failures rest at 1431-1436). A stream succeeds when its first item arrives without error, while a stream error before or after that item records a failure. A revision captured before the call guards success recording, so an older success cannot clear a failure, panic, or observation that landed during the call.
- A contained panic records a timed generic-ladder cooldown, advances the member's failed generation, and republishes a snapshot without that member. A task waits until the rest ends and republishes the member only if its generation is unchanged. A newer panic therefore keeps the member out and doubles the cooldown instead of allowing an older timer to restore it.
- Passive observation uses a per-member `ObserveBackend` bound by the daemon and passed through `hofund_plugin_host::runtime_host_with_observe`. The production backend calls `Cooldowns::observe` with the `QuotaObservation` for that member. Plugins submit observations but cannot name or select a member. `QuotaObservation` separates `retry_after` from `window_reset` and carries `remaining`, `limit`, and `spending_credits`. Zero remaining uses a positive window reset capped at eight days, then a retry hint capped at one hour, then a 15-minute fallback. Positive remaining at or above 98% used with a known positive limit rests only when a positive window reset is known, capped at eight days; a retry hint alone or an absent reset records nothing and does not advance the revision (reference: internal/provider/routing.go:365-375). An account spending credits does not rest. Other observations also return without recording anything or advancing the revision (reference: internal/gateway/routing.go:1004-1017, reference: internal/gateway/routing.go:1291-1304, reference: internal/provider/codex_switch.go:48-62). The ordered strategy's 100% exception remains with the routing-strategy work in row 4.5.
- Provider execute and count status errors and SDK status errors parse `Retry-After` as delta-seconds or an HTTP date. A zero delta or a date not strictly in the future is absent. Transport parsing deliberately applies no cap; the cooldown book caps every header hint at one hour for every cause, including quota (magpie `routing.go:166,1432-1435` at `5b4da5b`). The eight-day cap applies only to vendor-stated resets from passive observations with an exhausted window.
- `Cancelled`, `Config`, `Unsupported`, and `Internal` are preserved across the plugin round trip and never cool. Request-shaped 4xx responses also deliberately do not cool a member, unlike the reference gateway's broader treatment. There are no per-provider cooldown configuration knobs yet; `CooldownPolicy::default()` supplies the policy. Exposing cooldown times in the routing view is deferred. Success-header vendor feeds are left to subscription plugins in H rather than adding vendor-specific parsing to the kernel.
- Two cooldown choices deliberately diverge from magpie. A new event never shortens an active rest, whereas magpie overwrites the deadline (`routing.go:1158`, `restingUntil.m[id] = r.Until`); this prevents a short hint from cutting a long quota rest. Credit rest is `max(30 minutes, header hint capped at one hour)` rather than magpie's fixed 30 minutes, so a longer vendor hint is honoured.

### PR L plan notes

The implementation plan is [docs/plugins/control-plane.md](../plugins/control-plane.md). It keeps the accepted crate, client, process, and reload boundaries, with these clarifications and deviations:

- The management API uses a separate loopback-only listener, not the model listener. The model gateway may bind a LAN address without exposing local control routes. Later LAN-facing remote usage and quota features stay on the model listener with gateway-key authentication; the control listener is never widened for them.
- Model picks are persistent host state, never TOML. GET and PUT `/v1/hofund/providers/{id}/picks` return `picks`, `picked_from`, `delisted`, and a revision that is the SHA-256 of the serialized in-memory provider state. PUT accepts a full list, only `+a` and `-b` changes, or the `all`/`-` reset with an expected revision; mixed forms and an empty edit result fail, and `none` waits for a distinct no-models flag. The daemon serializes picks and refresh on the `ModelsHost` state mutex through a new method that holds it across load, revision compute, compare, apply, and save; it returns 409 for a stale revision and 500 when the save fails. One-shot picks and refresh reread and write under a per-provider `<state-file>.lock` with ordered runtime fallback and atomic replacement. `provider set id=` is rejected until rename lands under feature-parity row 11.6. Bare `provider models <id>` performs the same refresh, delist, auto-include, and save operation as `refresh` and `fetch`.
- Reload is triggered by a control call or the L6 watcher, which watches config and resolved-target parent directories, canonicalizes both sides of its exact-path filter, debounces matching events, forces a stat-and-revision check on overflow or rescan, and retains a 2 s stat-polling fallback. Validation and candidate construction finish before one ArcSwap publication. Port changes bind before closing and same-port host changes close first; a relisten failure publishes the rest, reports `listen` in `not_applied` and an event, and is retried by explicit reload even at equal revisions. `pending_restart` is recomputed on every publish.
- The model listen address resolves through one function used by `hofundd` and healthcheck: `HOFUND_ADDR` first, otherwise config `listen`. Only the daemon binds it: `hofund serve` execs `hofundd`, and one-shot operations never bind. Status reports configured, effective, and running addresses and the effective-address source. A config listen change under the environment override publishes without relisten and reports `not_applied` with reason `env_override`. Healthcheck maps `0.0.0.0` and an empty host to `127.0.0.1`, but `[::]` to `[::1]` because an IPv6-only bind may not accept IPv4 loopback.
- SIGHUP drains, flushes, and re-raises with the default disposition on Unix under foreground `hofund serve`; it is not a reload signal, and drain logging tolerates closed output streams. On Windows, Ctrl-C and Ctrl-Break drain and exit 130, while close, logoff, and shutdown exit 143. Drain is best effort for those three events because Windows terminates the process about 5 s later.
- SSE is the event transport. Subscription to each requested topic precedes the snapshot. Control and request topics have independent bounded broadcasts, replay rings, capacities, and sequence spaces; request events are opt-in, and lag resynchronizes only the affected topic. Every frame id has grammar `boot-id ":" cursor ("," cursor)*`, with `cursor = topic "=" sequence`, canonical topic order, and every subscribed topic's latest cursor. A standard EventSource reconnect therefore resumes all topics. An unknown boot id resynchronizes all requested topics, while a topic missing from an otherwise valid Last-Event-ID resynchronizes only that topic. Events are not durable across restarts.
- Daemon locks and discovery files are keyed by canonical config id. Cross-process config reloads and writes use a stable resolved-config sidecar lock, never the config inode. A read-only config directory falls back to the ordered per-user runtime locations for daemon lock, discovery, and `config-<id>.lock`. Reload proceeds under that fallback lock; a config write still fails clearly with the read-only directory in the error and no partial write.
- Socket handoff and background-service installation are deferred to feature-parity rows 16.3 and 16.4 (`docs/feature-parity.md:329-330`). This narrows the placement-table sentence that assigned them to L because listener transfer and service management are independent of the client boundary.
- After L the CLI's workspace dependencies are `hofund-config`, `hofund-control-client`, and `hofund-secrets`, plus the external `toml_edit`; it no longer depends on `hofund-agent` or `hofund-daemon`. Most runtime commands use one-shot `hofundd <op> --config ...` only when the per-config daemon lock is free at every lookup location. The probe opens locks without create and creates no directories. While a lock is held, discovery retries for at most 5 s; a still-held lock then fails with its path, the PID or `unknown`, and a starting-or-unresponsive explanation, never one-shot fallback. One-shot processes never take the daemon lock, bind, write discovery, or start the watcher; config writes use the sidecar lock and revision recheck. `events tail` autostarts the daemon and retries, `healthcheck` directly probes the model `/health` and fails when no model gateway answers, offline `config reload` is a successful reported no-op, and offline `status` is a local config report. This preserves offline agent operations, provider operations backed by current config or host state, usage, and full config validation without moving registry linkage out of `hofundd`. Only a missing `hofundd` permits a clearly labeled structural-only config-check fallback. `hofund-secrets` serves local `token show`, which has no control route.
- L's reference citations use `dc1cfe0`; earlier rows remain pinned to `cbbf316`.
- The CLI stays at `crates/cli` until row J. Desktop removal of the embedded daemon remains a follower of L.

### PR L1 notes

L1 adds `hofund-control-proto` 0.1.0 with versioned DTOs, all route request and response types, event envelopes and payload variants, bounded decoding, composite SSE ids, resume planning, JSON snapshots, and a source-walked public-API snapshot. It has no HTTP, Tokio, daemon, router, provider, or plugin-api dependency.

- R1: `ControlErrorKind` adds `BadRequest`, `UnsupportedVersion`, and `Unprocessable` before `Unknown`. `http_status()` supplies the default status, including 400 for the first two and 422 for semantic validation and configuration validation. There is no rate-limit kind; an upstream 429 stays `Upstream` with status 429 selected by the daemon mapping.
- R2: `FieldIssue` carries a field, a stable snake_case reason, and a sanitized message. The same type serves validation details and `not_applied`.
- R3: configuration text and rotated tokens have manually redacted `Debug` implementations. Configuration debug output shows only its byte count; token debug output is always `<redacted>`. Snapshot values are explicitly fake.
- R4: every response and event struct implements `Default`, with `version = API_VERSION`; body request constructors set the current version. Integration snapshots construct values only through that public API. `ConfigResponse` also enforces the decoded text limit on response decode.
- R5: `EmptyRequest` is a unit-like no-body marker. It is not a JSON request-body type and is never decoded by `decode_request`; `UsageQuery` is likewise query-only and uses the path version.
- Boot ids are tightened to nonempty ASCII graphic bytes (`0x21..=0x7e`) except colon, comma, and equals, up to 64 bytes. This rejects control bytes that SSE ids cannot carry as well as whitespace. Composite-id construction and parsing share the same canonical cursor rules. Ring-retention decisions remain with the L6 server.
- Stable provider-error and cooldown mappings live in the daemon's hidden public `control_map` module because that crate owns both internal and wire types. Upstream 429 is the only status-preserving special case. Retry durations round up to whole milliseconds and saturate at `u64::MAX`. The gateway receives its proto edge in L3, not L1.
- The cooldown wildcard arm in `crates/daemon/src/control_map.rs` maps future causes to `CooldownReason::Unknown`; this is accepted for L1, with #187 tracking an exhaustive mapping.
- The manual `EventPayload` deserializer in `crates/control-proto/src/events.rs` maps unknown event tags to `EventPayload::Unknown` for forward compatibility.
- The unit `Unknown` variants in `crates/control-proto/src/events.rs` and `crates/control-proto/src/error.rs` carry no payload by design; they are fallback markers, not containers for unrecognized data.
- The Windows spike found no maintained, license-compatible safe crate covering both protected current-user pipe creation and connected-client SID lookup. Interprocess 2.4.7 lacks both safe security operations and uses `0BSD OR Apache-2.0`, whose 0BSD option is outside the current license allow list; windows-permissions and windows-acl are stale and incomplete; sddl is GPL-3.0; Tokio and windows-sys require unsafe FFI. Windows stays on authenticated loopback TCP. L4 will isolate `hofund-platform-win`, resolve SIDs through process tokens, create exactly `D:P(A;;GA;;;<current-user-SID>)` with Tokio's raw security-attributes hook, verify the client through `GetNamedPipeClientProcessId` and its process token, and require Windows CI DACL and wrong-SID tests. The detailed evidence and links are in the external L1 spike report.
- L3 owns HTTP routing, TCP authentication, and the gateway/client edges. L4 owns Unix and Windows transports and the isolated platform implementation. L5 owns stateful route operations and mapping call sites. L6 owns reload, watcher, listener, event buses, replay rings, retention checks, and SSE production.

### Parallel work plan

B is alone. After B, B2 and C can proceed together. D starts after C, because both edit `providers` and C lands the snapshot those calls publish. K waits for C and must merge before H. L waits for D. H does not wait for L: until L, subscription commands stay on the in-process CLI path. H does wait for encrypted account storage. The landed `AccountStore` is the store those plugins persist to.

After D, four batches run at the same time. H waits until K has also merged. Counts are pull requests, one directory each unless the row says otherwise.

| Batch | Starts after | Pull requests | Concurrent agents | Merge constraint |
| --- | --- | --- | --- | --- |
| E providers | D, B2, and the fragment-format pull request | About 8. Four code plugins (azure, vertex, bedrock, mantle) and about four data-only fragment batches (URL-only presets on `openai-compat`, `anthropic`, and `generative`, plus the six ids `resolve` still rejects). | Up to 4, one directory each. | Sorted append to `plugins/registry`. On conflict, keep both lines and re-run `xtask`. I waits until every E pull request has merged. |
| S inbound surfaces | D, including the gateway mount | 5: Ollama, Muse, Codex backend, embeddings, audio. | Up to 5. | Same registry rule. The Codex subscription waits for the Codex surface. |
| F and G agents | D. Codegen is in D, so F is the four migrations, not a loader. | F is 4. G is about 12 (one `FileKind::Yaml` change, then pi, goose, zed, vscode, crush, hermes, kimi, qwen, droid, cline, grok). | The four F migrations and the non-YAML G files together. Goose and hermes wait on YAML. Zed is code when `custom_write` is required. | Agent directories are disjoint. YAML is the one shared kernel edit. `CatalogModels` and `ModelTier` are in the engine from D, not a per-agent pull request. |
| H subscriptions | D, B2, and K. Codex also after S. Not L. | 4: codex, claude, copilot, gemini. N9 is a fifth, kernel-API, pull request immediately before Copilot. | 4 plugin agents, after N9 for Copilot. | Same per-kind registry rule as E. The Codex subscription may share the agent id; its Cargo package and registry feature use distinct build identifiers. They do not edit the router. Cooldown is already in K. Sign-in uses the in-process CLI until L. |

L runs beside E, F, G, and S. The desktop rework does not start until L has merged.

Pull request #28 does not join a batch. It rebases onto B and merges only after the trait exists. It keeps the in-process CLI path until L, so it does not wait for the control plane.

Assign one agent per directory. Do not assign two agents to the registry crate. The kernel owner merges B, B2, C, D, K, L, the YAML change, I, and J.

The six presets that `resolve` still rejects (`typesafe`, `vercel-jev`, `cloudflare-jev`, `bailian-decision`, `qwen-decision`, `remote-gateway`, from [providers.md](../coverage/providers.md)) are fragment or code pull requests in E. They are not a reason to keep `presets.toml` inside the kernel.

### Who owns which path

One agent per batch. A batch is a set of directories that do not overlap.

The only shared file in a batch is `plugins/registry/Cargo.toml` (features and dependencies, sorted, append-only) and the generated registry, which is rebuilt rather than hand-edited. Two agents appending different lines produce a one-hunk conflict. The resolution is to keep both lines, sort them, and re-run `xtask`. Nobody hand-edits a `match` inside `crates/daemon` or `crates/kernel`.

`CODEOWNERS` is added by the first migration pull request, not by this one. This document does not name GitHub users or teams. The owner fills the handles. The path globs are:

```text
/crates/plugin-api/
/crates/plugin-sdk/
/crates/plugin-host/
/crates/control-proto/
/crates/control-client/
/crates/kernel/
/crates/daemon/
/crates/router/
/crates/gateway/
/apps/hofundd/
/apps/cli/
/plugins/registry/
/plugins/providers/
/plugins/subscriptions/
/plugins/agents/
/plugins/inbound/
/kits/
/fuzz/
```

Kernel globs, `plugins/registry`, the control crates, and `fuzz/` belong to whoever the owner assigns to the kernel. Each plugin batch's pull request adds the lines for the directories it introduces. A plugin agent does not need write access to another plugin's directory.

`docs/magpie-comparison.md` gains the adopt-or-redesign rows for this seam in pull request B. This pull request does not edit that table. The comparison is recorded when the seam lands.

### PR H0g-a notes

- `HttpHandle::send` on the pooled host streams a 2xx body as the upstream sends it. A read failure after the headers is one `Interrupted` chunk, and then the stream ends. An empty 2xx body yields no chunks, where it used to yield one empty chunk.
- A non-2xx body is still buffered. A read break while buffering keeps the bytes that arrived, so a status error keeps its body and `Retry-After`.
- Dropping the body drops the upstream response and closes its connection.
- SDK `Call::send` still reads the whole body through `read_body`, so its replies and retry rules do not change.
- H owns row H0g (ruling Q2). The SDK protocol transports, the removal of the raw-client method and the kit conformance test over the plugin transport are deferred to H0g-b. The Chat and Responses transport lands as the SDK `wire` module in PR E8a.

### PR H0a notes

- `AccountsHandle::lease(id, wait)` takes an exclusive per-account lock in a mode `0600` sibling file, then reads the row and its secret past the cache. A refresh holds the lease, calls the vendor, and calls `commit` with the new secret. Dropping the lease without a commit releases the lock and writes nothing. Waiting longer than `wait` is an `Io` error; a missing account is a `Config` error.
- Lock order is lease, then store. `upsert` takes the lease (bounded at 30 s) so it cannot overwrite a refresh in flight; `remove` does not. A commit after a removal is an error and does not bring the account back.
- The lease file closes on drop and unlocks explicitly, because a forked child holds the open file description until it execs. Lease files persist. Account ids are sanitized into the file name, so two ids that differ only in unusual characters share one lease, which only over-serializes them.
- `SchedulerHandle::run_once` runs one named background job. It keeps running when the caller drops its future, and it is cancelled by `cancel` or host shutdown.
- `QuotaWindow` gains `blocking` (default `true`) and `scope_model`. `SignInCtx` gains a session (32 lowercase hex digits, set by the host) and named options. `SignInProof::Await` reports a sign-in that completes later. `Subscription` gains `cancel_sign_in` and `account_status`, with defaults of no-op and `Ready`.

### PR E8a notes

- New SDK module `hofund_plugin_sdk::wire` and the edge `plugin-sdk -> codec`. `wire::execute` encodes a Chat or Responses request, sends it on `HttpHandle`, and returns raw bytes for the same protocol or decoded events across protocols. Cross-protocol calls drop request extensions, as the adapter does.
- The builtin `openai-compat` factory uses `wire::execute` unless the base is a Bedrock Mantle root (`OpenAiCompat::direct_base`). Model lists and probes stay on the adapter transport.
- Only a connection-establishment failure is retried, once. A reset after the write is `Transport` and is not replayed. The adapter labels the same failure `Connect`, which the router fails over; that label is a follow-up in `hofund-provider`.
- A leading in-band error event fails the call before any event is yielded, and a non-2xx status keeps its `Retry-After`. Error messages are capped at 512 bytes.
- `hofund-kit-upstream` re-exports `runtime_host` and `HttpLimits` so registry tests can run both transports on the pooled client against a mock.

### PR H0b notes

- `HostContext::loopback()` returns a `LoopbackHandle` over the `#[doc(hidden)]` kernel trait `LoopbackBackend`. Hosts without a listener report `Unsupported`. The handle checks the sign-in session, the plugin id and the spec before the backend sees it. `bind` takes `&LoopbackSpec` rather than a value, since nothing in it is consumed.
- `hofund_plugin_host::LoopbackRegistry` is the backend. It listens on `127.0.0.1` only (`SO_REUSEADDR` on Unix only, so a fixed port is not shared on Windows), reads at most 8 KiB of request head within 10 s per connection, and serves each connection on its own task, so an idle connection does not block the callback. GET and HTTP/1.x only.
- Routing: `/cancel` cancels without a `state` (as the reference does). Any other path than the bound one is 404. A wrong `state` is 400 and the listener keeps waiting. The first matching request claims the sign-in; later ones get 409. `error=` fails the sign-in and the error token is repeated only if it is short and plain. Replies carry `no-store`, `Referrer-Policy: no-referrer`, `nosniff`, `Connection: close`, and never the code. The listener closes 1 s after the claim.
- A fixed port that is busy (`AddrInUse`) reports `Busy`, and one that is not permitted reports `Reserved`; either way the sign-in carries on paste-only. Per the design divergence, the kernel never sends `/cancel` to a foreign listener.
- The kernel awaits the result with `LoopbackRegistry::await_callback(session, paste, deadline)`, a method rather than a free function so the registry is explicit. A pasted line is checked with `sign_in::parse_pasted_callback`, which lives in the kernel next to the listener rather than in SDK `oauth/callback.rs`: a pasted provider error fails the sign-in, and anything else that does not match is ignored. The deadline is a `Timeout` error and closes the listener; the session entry stays until `release`, so a second wait is refused.
- SDK `oauth::Pkce::generate` (S256, 32 random bytes), `oauth::new_state` (32 random bytes, base64url) and `oauth::authorize_url` (https, or http on a loopback host only; no fragment or user info).

## 7. Comparison with magpie at `cbbf316`

Pins: hofund `dfb6e89e64b1eb5d9b489b7f7af2f0685bac41f4`, magpie `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`.

### What we adopt

| Magpie | Where | Hofund |
| --- | --- | --- |
| The runtime never imports a plugin, and a plugin never imports another. They meet through services. | `internal/pluginrt/pluginrt.go` lines 8-10. Test: `internal/pluginrt/pluginrt_test.go` line 94. `main.go` and `plugins/all` are the only gatherers (test comment, line 91). | The registry crate is the only gatherer. `xtask` rejects a plugin-to-plugin dependency. In the end state `hofundd` is the only composition root. Plugins meet the kernel through `HostContext`. |
| One plugin per provider id, and a recorded clash when two claim it. The host runs the user's choice, otherwise the last one listed. | `internal/plugin/host.go` `Loaded.Provides` line 499, `Clash` lines 509-517, `Clashes` line 521. | The registry rejects a duplicate preset id at build time. Stage 1 has no user-installed overlay, so a duplicate is a hard error rather than a runtime preference. |
| A plugin agent declares the file, the format, the model key, and the prefix. The host performs every write and stashes the user's previous values. | `internal/agentplug/agentplug.go` `Desc` lines 65-83, `Formats` lines 57-63. `internal/agent/plugged.go` lines 3-10 and `pluggedStash` line 48. A plugin id that collides with a built-in is the built-in (`plugged.go` lines 31-34). | `agent.toml` compiled by `build.rs`, plus the engine in `hofund-agent`. A data plugin does not write files itself. A colliding id in the agent kind fails the registry check; equal ids in other kinds do not collide. |
| An API version constant on the manifest. | `internal/pluginrt/pluginrt.go` line 27, `const API = "1.0"`. | `API = "1"` is the future process-host handshake. In-tree compatibility is `#[non_exhaustive]` plus the public-API diff (section 1). |
| Presets are data. | `internal/provider/presets.go`, recorded as `crates/provider/presets.toml` (58 presets, `source_commit` at line 8). | Preset fragments stay TOML and are compiled to static data. The implementation crate is shared. |
| A failed built-in does not take down the rest. | `pluginrt.go` lines 1-6. | `catch_unwind` per member (section 1). C marks the member failed and drops it from the next snapshot. K adds the timed cooldown. The process stays up. |
| Probe and list live next to the provider. | `internal/provider/test.go` `Provider.Test` line 37, `AuthHeaders` line 457, `probe` line 500. `internal/provider/autolist.go` `MinRefreshEvery` line 39 (10 minutes), `listRetry` line 231 (15 minutes), `KeepListsFresh` line 239. | The methods are `UpstreamProvider::probe` and `list_models`. Timers go through `HostContext::scheduler`. Pull request #28 stays unmerged until the methods exist. |
| Schema-driven UI parts. | `pluginrt.go` lines 29-60, `Parts`. | `config_schema()` and `SignInChallenge`. The client renders them. It does not link the plugin. |

### What we redesign

| Magpie | Where | Hofund |
| --- | --- | --- |
| Product plugins under `plugins/` (budget, routing, otel, middleware, sessions, tools, usage). | `plugins/*`, gathered by `plugins/all`. | Section 2.4 places each one: kernel layer, kernel service, inbound plugin, or a later RFC. They are not stage-1 plugin kinds, except inbound surfaces. |
| Community auth plugins run in-process JavaScript. | `internal/agentplug/agentplug.go` lines 18-19. Coverage line 16. `internal/plugin/host.go`. | Stage 1 is native Rust, compiled in. The 13 community subscriptions are a later process host. WASM is the wrong vehicle for those JavaScript plugins. |
| Sign-in, presets, probes, and every vendor client share `internal/provider`. | `internal/provider/signin.go`, `presets.go`, `test.go`, `autolist.go`. | Transports, SigV4, and OAuth helpers live in the SDK and call the kernel client. Vendor URL tables are compiled fragments. Vendor credential code is a plugin crate. The kernel match in `crates/daemon/src/lib.rs` lines 251-285 goes away as those plugins land. `Subscription::upstream` returns the client. The account store holds the secret. |
| Agent plugins are JavaScript modules with a 1 second load limit. | `internal/agentplug/agentplug.go` lines 1-4, 119-120. | Declarative TOML compiled by `build.rs`. No embedded interpreter in stage 1. |
| `plugins.yml` can turn a built-in off. | `pluginrt.go` lines 3-6 and 84. | Stage 1 links the in-tree set. A feature flag on the registry is how a developer builds a subset. Per-member enable in config is follow-up N8. Users do not unlink a crate. |

## 8. Follow-ups

These are not required before acceptance. They have homes so they are not forgotten.

| Id | Follow-up | When |
| --- | --- | --- |
| N2 | Add a capability set on `UpstreamSpec`: `count`, `list`, `probe`, `quota`, `passthrough`. The UI reads it instead of calling the method. | With or just after B, before a settings UI depends on it. |
| N5 | A `[capabilities]` table in `plugin.toml`: network hosts, filesystem paths, spawn. Reviewers check it. The future process host enforces it. | Before the process-host RFC, and earlier if the owner wants the review aid. |
| N8 | Per-member `enabled` in config. A disabled member stays linked and is left out of the snapshot. This is the desktop equivalent of magpie's `plugins.yml` toggle, at member granularity. | With L, before the desktop settings form. |
| N9 | `SignInFlow::Device` takes the host from validated options (`DeviceFlow { host: Url }`), not a `'static` URL, so GitHub Enterprise is expressible. Import and discovery stay the kernel `AccountSource` and the `import_providers` hook from section 2.4. | N9 merges before the Copilot pull request in H. |

## Sign-off

Accepted on 2026-10-25. Owner sign-off is conditional on the architecture review (appendix), including wording fixes F1, F2, and F3. Acceptance means the owner agrees to the kernel boundary, `HostContext`, the factories, the four kinds, the lifecycle and snapshot rules, the control plane, the placement table, and the migration order. It does not merge any of the migration pull requests by itself. PR B starts only after this pull request is merged.

Until the migration pull requests land:

- `docs/architecture.md` is the build.
- Coverage cells and feature-parity statuses stay as they are.
- Pull request #28 stays unmerged until the trait it implements exists.

## Appendix: Review resolution

Review file: architecture review of head `b93d1c2488c7b29ee7af5ea7dd0c4b955d37f732`, the re-review of `f372e4c`, and the final check of `a9fc60f`. Each id points at the section that resolves it. F1, F2, and F3 are the wording fixes from that final check.

| Id | Where it is resolved |
| --- | --- |
| M1 | Section 1, "Host context". Handles: `http`, `secrets` (`SecretString`), `accounts`, `clock`, `scheduler`, `spawn_process`, `cache_dir`, `state_dir`, `observe`. Construction is R2. |
| M2 | Section 2.1 `UpstreamFactory` (associated `Config`, erased `DynUpstreamFactory`, no downcast, `From<ConfigError> for BuildError`). Section 2.1 "Config shape" and PR B2, including `[subscriptions]`. `SubscriptionFactory`, `AgentFactory`, and `DynAgentFactory` in sections 2.2 and 2.3. `SubscriptionSpec` is in the type list. Instance-level `spec` removed. `protocol` stays a `String` (R3). |
| M3 | Section 1 pooled client. Section 4 rules 1 and 9: no `reqwest`, `hyper`, `axum`, `tokio` net or process, `std::process`. SDK only. |
| M4 | Section 2.3. API trait is `description`, `custom_write`, and `import_providers`. `GatewayTarget` and `Plan` live in the API. `TierId` is defined. Engine stays in `hofund-agent`. `Slot::ModelTier`. `build.rs` static data. PR F wording in section 6. |
| M5 | Section 1, "Contract types" and "Semver policy". `#[non_exhaustive]`, private fields, newtypes, `SurfaceId` instead of `KernelRoute`, `ProtocolId`, `Send + Sync + 'static`, named IR re-exports. `const fn` constructors are R7. Handle construction is R2. |
| M6 | Section 1, "Errors". One `PluginError` with `kind`, `status`, `retry_after`, `source`. `unsupported` exists. `probe` has a default body in section 2.1. Router wildcard in PR C. |
| M7 | Section 1, "Lifecycle, isolation, and reload". `validate`, `build`, `health`, `shutdown(deadline)`, including `InboundSurface`. Kernel timeout and `catch_unwind`. C marks the member failed and drops it from the next snapshot. K adds the timed cooldown. Cancel-safety. Protocol error event instead of EOF at gateway lines 573-576. |
| M8 | Section 1, `ArcSwap` snapshot, stable `MemberId`. Section 2.2 membership "at startup and on every membership change". Secret read per call, not captured. |
| M9 | Section 1 `observe` and `retry_after`. Section 2.2 corrects the cooldown claim against `architecture.md` line 151. PR K before H. |
| M10 | Section 1, "Observability". Kernel spans and metrics. `tracing` only via the SDK. Redaction at the layer. `tracing` added in B and wrapped in C. |
| M11 | Section 3, "Control plane", and section 4. After L, `apps/hofundd` (`hofund-daemon-bin`, binary `hofundd`) is the only direct registry linker, and the library takes the registry as an argument. The CLI does not depend on the registry directly. Until L it links it transitively through `hofund-daemon` (F1). `hofund serve` execs `hofundd` after L. Desktop and TUI use `hofund-control-client`. `config_schema` and `SignInChallenge`. CLI at `apps/cli`. Until L, provider and subscription commands stay in-process (R6). PR L before the desktop rework. |
| M12 | Section 2.4, one row per capability in the review table, plus the `InboundSurface` trait and the PR D mount (R1). |
| N1 | Section 1, "Same-protocol passthrough". `Completion::Raw`, usage tap, no `model_request.clone()`, `criterion` benches, magpie baseline as a goal. Lands in PR C. |
| N2 | Section 8. Not required for acceptance. |
| N3 | Section 5. Mandatory `hofund_kit_*::conformance`, `xtask` check, cancellation, deadline, mid-stream error, redaction, concurrency, per-OS goldens, `fuzz/` targets. |
| N4 | Section 4, "Lints and feature checks". `deny(missing_docs, unreachable_pub)`, pedantic warn, public-API snapshot, `cargo hack --each-feature`. |
| N5 | Section 8. The privilege statement is in section 1 so the gap is not silent. |
| N6 | Section 1, "Kernel `Member`". Router does not depend on `UpstreamProvider` or on `tower`. `Member` stays `#[async_trait]` for that reason. It does depend on `MemberId` as well as `PluginError`, `Request`, and `Completion`. The daemon adapts `Member` into a `tower::Service` for the layer chain. PR C. |
| N7 | Section 1, "Semver policy". In-tree `#[non_exhaustive]` plus public-API diff. `API = "1"` is the process-host handshake only. |
| N8 | Section 8. Also the on/off row in section 2.4. |
| N9 | Section 8. Must merge before Copilot in batch H. GHE lives in `options` until that enum change (section 2.2). |
| N10 | Workspace version assertion kept in the header. `Wire` rationale stated once (section 1, coupled paragraph points at `ProtocolId`). Types left to PR B are listed in section 1, "Errors", including `GatewayTarget`, `Plan`, `TierId`, `ModelSpec`, `Body`, `BodyStream`, `SignInFlow`, `SignInChallenge`, `SubscriptionSpec`, `RouteSpec`, and `SurfaceHost`. `AccountPool` is removed in favor of the account store. `From<ConfigError> for BuildError` is stated there. |
| R1 | Section 2.4, `InboundSurface` (`id`, `routes`, `handle`, `health`, `shutdown`). PR D mounts routes (conflict check, auth, spans, timeout). PR S depends on that mount. `SurfaceId` replaces `KernelRoute`. `requires` is checked by `build.rs`. No vendor-scan allowlist (rules 5 and section 1). |
| R2 | Section 1, "Host context". Handles wrap `Arc<dyn Backend>`. Backends and the only constructor live in `hofund-plugin-host`. Rule 13 forbids a normal dependency from `plugins/`. Kits may depend on the host crate and expose `test_host` (F2). "Sealed" is not used for handles. |
| R3 | Section 2.1, "Config shape", and rule 7. Config stores `protocol` as a `String`. The daemon uses `ProtocolId::try_from`. `Kind` is deleted rather than turned into a config type. |
| R4 | Section 3 and section 4. Package `hofund-daemon-bin` at `apps/hofundd`, binary `hofundd`. `hofund serve` execs it. Rule 3 and the edge list name that package. |
| R5 | Section 2.3. `GatewayTarget` and `Plan` move into the API (from `crates/agent/src/lib.rs` lines 67 and 174). `import_providers` is on the trait. `TierId` is defined in section 1. |
| R6 | Section 6. Until L, provider and subscription CLI commands, including #28, stay in-process and do not wait for L. `[subscriptions]` and `[subscriptions.options]` are PR B2. C is ordered before D. C marks a member failed; K adds the timer. |
| R7 | Section 1, "Contract types". `#[doc(hidden)] pub const fn` constructors for every `static` the registry emits. |
| F1 | Section 3 (composition root and the CLI bullet), rule 3, and the CLI edge. The CLI and the desktop do not depend on the registry directly. Until L they link it transitively through `hofund-daemon`. After L the library takes the registry as an argument. The ban, including transitive links via `cargo metadata`, turns on in L. |
| F2 | Section 1, "Host context", rule 13, and the host edge. Kits (`publish = false`, test-only) may depend on `hofund-plugin-host` and expose `test_host(...) -> HostContext`. Rule 13 restricts only normal dependencies from `plugins/`. |
| F3 | Section 4, Names and rule 3. The rule is by path (`plugins/<kind>/`). Plugin ids `api`, `sdk`, `host`, and `registry` are reserved, so `hofund-router` depending on `hofund-plugin-api` and `hofund-daemon-bin` depending on `hofund-plugin-registry` are legal. |
