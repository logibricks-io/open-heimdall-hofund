# Subscription parity checklist (R-46)

Per-vendor, code-level parity audit of hofund subscription plugins against the frozen reference baseline **v0.1.3005** (tag commit 6abacd46; Go core under `internal/provider`, `internal/gateway`, `internal/claudebridge`) and the community auth plugins it ships with (the reference's community plugins repository at 1ad712d, the last commit before the v0.1.3005 tag). hofund side: main at **08035c9**.

Scope: the 14 subscriptions in the v0.2.0 plan (Claude Pro/Max, Cursor, Devin, Zed, ChatGPT/Codex plus ChatGPT API, Copilot plus GHE, Gemini Code Assist plus Antigravity, SuperGrok, Kiro, Factory, Cline, Qoder plus CN, CodeBuddy/WorkBuddy plus international, Z Code). Each section covers rows a-q: sign-in methods, import, token storage, refresh, upstream wire, inbound API shapes, streaming, tools, images, reasoning, quota, model list, multi-account, error classification, warm-up/check-in, agent sign-in switching, vendor extras.

This is a code-level checklist. Real-account smoke tests are out of scope and happen after the release candidate. Rows marked "unverified" are places where the audit could not confirm behaviour from code alone; the implementing task must confirm them.

Legend: **Done** = matches the reference at checklist granularity; **Partial** = subset, or mocks only; **Missing** = not implemented; **N/A** = the reference does not offer it.

## Summary

| Vendor section | Done | Partial | Missing | N/A | Unverified |
|---|---|---|---|---|---|
| Claude Pro/Max (`claude`) | 1 | 17 | 3 | 2 | 0 |
| Cursor (`cursor`) | 8 | 11 | 0 | 2 | 0 |
| Devin (`devin`) | 12 | 6 | 0 | 1 | 0 |
| Zed (`zed`) | 0 | 6 | 1 | 3 | 0 |
| Kiro (`kiro`) | 8 | 9 | 0 | 2 | 0 |
| Factory / Droid (`factory`) | 14 | 2 | 0 | 3 | 0 |
| Cline / ClinePass (`cline`) | 12 | 4 | 0 | 3 | 0 |
| ChatGPT / Codex (`codex` backend) and ChatGPT API (`chatgpt-api`) | 10 | 10 | 1 | 0 | 0 |
| GitHub Copilot (github.com and GHE) | 15 | 2 | 0 | 5 | 0 |
| Gemini CLI (Code Assist) | 13 | 3 | 0 | 2 | 0 |
| Google Antigravity | 11 | 4 | 1 | 1 | 0 |
| SuperGrok (`grok`) | 14 | 2 | 0 | 2 | 0 |
| Qoder (international + Qoder CN) | 13 | 5 | 0 | 2 | 0 |
| CodeBuddy / WorkBuddy (CN + international) | 18 | 1 | 0 | 0 | 0 |
| Z Code (`zcode`) | 16 | 3 | 0 | 0 | 0 |

Gaps that had no plan item before this audit (the supervisor's v0.2.0 plan and backlog now carry them under the item named):

- Claude: WSL fallback and per-login proxy for Claude Code sign-in and runs — **M**, folded into **R-6** (the bridge runs Claude Code, so the launcher owns both).
- Claude: setup-token (`sk-ant-oat...`) inference-only accounts — **S**, added to **R-5** scope.
- Devin: CLI install/bootstrap helper (**R-12**) and keepalive (**R-13**) — **S** each.
- Codex: detect Codex CLI API-key mode and skip it in discovery and switching — **S**, added to **R-47** scope.
- Cursor: `CURSOR_API_KEY` exchange sign-in and team billing header `x-cursor-team-id` — **M** + **S**, added to **R-8** scope.

Every other gap below maps to an existing plan item.

---

## Claude Pro/Max (`claude`)

**Reference files read:** `internal/provider/claude_signin.go`, `claude_auth.go`, `claude_dirs.go`, `claude_setup_token.go`, `claude_limits.go`, `claude_usage_cli.go`, `claude_warmup.go`, `claude_warmup.go`, `claude_probe.go`; `internal/gateway/claude_subscription.go`; `internal/claudebridge/mcp.go`.

**Hofund path:** `plugins/subscriptions/claude/src/` (`lib.rs`, `cli.rs`, `creds.rs`, `account.rs`, `agent_sign_in.rs`, `discover.rs`, `import.rs`, `lock.rs`, `upstream.rs`, `bridge/`).

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: browser / loopback via Claude Code | `startClaudeSignIn` runs `claude auth login --claudeai` in a temp dir, relays URL via `TookOpenedURL` / `openedURLEnv` (`claude_signin.go:147-198`) | `lib.rs` + `cli.rs` + `wslrun.rs` + `launcher.rs`: same command in private dir, link relay + optional browser wait; WSL fallback when Windows has no local `claude` (running distros only, mocked probes) | Partial | Per-login Bun proxy on sign-in (`netproxy.EnvForBunWith`) still Partial; `TookOpenedURL` / opened-URL env not ported; MCP bridge runs still native-only (`upstream.rs` `wsl_distro: None`) |
| a2 | sign-in: pasted device/browser code | Reads code from sign-in flow stdin when link shows a code (`claude_signin.go:147-151` comment) | `SignInProof::Paste` writes code to child stdin (`lib.rs:342+`, tests in `tests.rs`) | Done | — |
| a3 | sign-in: device code (standalone) | N/A — always mediated by Claude Code OAuth | N/A | N/A | — |
| a4 | sign-in: API key | N/A for subscription OAuth | N/A | N/A | — |
| a5 | sign-in: setup-token paste | `AddClaudeSetupToken` / `claude_setup_token.go:90+` accepts `sk-ant-oat…` into reference-owned dir | unverified — no `setup-token` path in `plugins/subscriptions/claude` | Missing | Port setup-token accounts (`claude_setup_token.go`) |
| a6 | sign-in: reuse desktop/CLI login | Reads live agent home when switching; discovery imports `.credentials.json` | `discover.rs`, `agent_sign_in.rs` with locks (`docs/coverage/subscriptions.md:105-120`) | Partial | macOS Keychain read/write for Claude Code credentials (`claude_dirs.go:42-58`) |
| b1 | import: agent files | Swap export + per-account dirs (`login_import` patterns; `import.rs` fixture shape) | `import.rs` parse swap export v1 (`import.rs:19-55`) | Partial | Additional reference import formats if any beyond swap v1 (unverified) |
| b2 | import: keychain / other tools | Keychain item `claudeDirService` (`claude_dirs.go:42-58`) | File-only `.credentials.json` in agent home | Partial | Keychain import path |
| c | token storage and encryption | Per-account config dir + optional macOS Keychain; the reference `logins.json` holds metadata (`claude_dirs.go`) | Encrypted account lease via daemon secret store; copies creds to account dir on use (`account.rs`, `creds.rs`) | Partial | Keychain parity; encryption model differs (hofund central store) — acceptable if behaviour matches |
| d | refresh | Claude Code refreshes; `invalid_grant` → Refused, not rested (`claude_auth.go:1-12`, `25-41`) | `account::refresh` via `claude auth status --json` (`lib.rs:403-404`, `account.rs`) | Partial | Map refusal strings to `NeedsSignIn` / Refused like `ClaudeSignInRequired` (`claude_auth.go:25-41`) |
| e | upstream request wire | No direct Anthropic HTTP; subprocess + MCP bridge (`claude_subscription.go:3-29`, `claudebridge/mcp.go:54-59`) | `upstream()` serves text, tools, images, and resume through Claude Code (`upstream.rs`, `bridge/`; PRs #753–#761 and R-6g). Tests use a fake runner. | Partial | Real Claude Code process and account (#304); WSL/proxy paths still unverified |
| f | inbound API shapes | Gateway `serveClaudeSubscription` for Anthropic Messages and translated Chat/Responses paths (`gateway.go:4995`, `claude_subscription.go:6041+`) | Gateway codecs already translate Chat/Responses/Messages to IR; the plugin serves those turns through the bridge | Partial | Real-account protocol smoke (#304) |
| g | streaming | Bridge streams Claude Code stdout/SSE to caller (`claude_subscription.go` run loop) | stream-json decoded to IR events, including `StillWorking` keepalive (`bridge/decode.rs`, `bridge/keepalive.rs`) | Partial | Live LongLivedChild keepalive wrapper and real-binary stream (#304) |
| h | tool calls | MCP `tools/call` blocks; results on next HTTP request (`claude_subscription.go:9-13`, `mcp.go:36-48`) | Tool parking plus MCP callback via `bridge-mcp-stdio` (`bridge/park.rs`, PRs #753, #756, #759) | Partial | Live helper + callback against a real Claude Code process (#304) |
| i | images | Gateway image tests pass through bridge (`claude_images_test.go` cited in tree) | Image blocks on the `-p` stream-json prompt (`bridge/images.rs`, PR #761). URL sources are not fetched; 2000 px / 5 MiB refit is not applied | Partial | Pixel fitting (#698); TUI URL fetch |
| j | reasoning / thinking | Effort / think-off **staged** in gateway (`6158-6161`, `6072-6076`, `anthropic.go:281-291`) | Gateway A–B extensions plus plugin CLI flags and `setEffort` (`bridge/flags.rs`, PR #761). Neutral **`X-Hofund-Cache-Marks`** instead of the reference vendor-prefixed cache opt-in header (`claude_cache.go:36-38`) | Partial | Real-account effort/`setEffort` (#304) |
| k | quota / usage | `claude -p /usage` parse (`claude_usage_cli.go:50-57`); plus `NoteClaudeLimits` from stream headers (`claude_limits.go:69-71`) | `quota()` unsupported (`lib.rs:407-414`) | Missing | `/usage` CLI + header-derived windows (`R-5`) |
| l | model list | From Claude Code / catalog in gateway context (unverified detail); usage scopes per model in limits | `list_models()` unsupported (`lib.rs:417-424`) | Missing | Model list + scoped week windows (`R-5`) |
| m | multi-account | Multiple saved logins, routing, agent switch (`claude_routed_signin_test.go` etc.) | Daemon multi-account + agent switch with locks | Partial | Quota-aware routing needs row k |
| n | error classification | `ClaudeSignInRequired`, refusal vs retry (`claude_auth.go`); subscription refusals move account (`gateway/refusal_accounts_test.go`) | `bridge/failure.rs` maps sign-in, extra-usage, too-long, quota, 429, and crash text; room-full 503 sets vendor-neutral `skip_failover_same_plugin` (R-6g). Sign-in lapses are pinned to the login version captured at run start | Partial | Real-account classification (#304); image/timeout live wrapping still thin |
| o | daily warmup | `KeepClaudeWindowsWarm` / `claude_warmup.go:77-79` | `daily_warmup` on spec + `account::warmup` (`lib.rs:54-63`, `454-459`) | Partial | Gateway-coordinated warm request path (probe) vs CLI-only warm-up |
| p | agent sign-in switching | Switch with locks, CAS on refresh token (`logins.go` patterns; docs in subscriptions.md) | `write_agent_sign_in`, discover/import (`lib.rs:437-475`) | Partial | Full lock parity documented in subscriptions.md gaps (overage quota) |
| q | vendor extras | WSL Claude, per-login proxy (`claude_signin.go:139-145`), setup-token inference-only | Sign-in, refresh, `auth status`, `/usage`, and warm-up go through `wsl.exe` when Windows has no native `claude` (`wslrun.rs` / `launcher.rs`; running distros only; mocked probes). Per-login Bun proxy, setup-token, and MCP bridge WSL (`upstream.rs` `wsl_distro: None`) are still absent | Partial | Per-login Bun proxy; setup-token (`R-5`); MCP bridge WSL |

### Gaps (Claude)

- MCP subscription bridge and multi-protocol gateway translation — implemented on the R-6 stack (PRs #753–#761 and R-6g), not merged; real-account smoke remains **#304**
- Quota from `/usage` + stream header limits — **L**, **R-5**
- Model list and scoped quota windows — **M**, **R-5**
- macOS Keychain credential path — **M**, **R-5**
- Setup-token sign-in and accounts — **S**, **R-5**
- WSL fallback for Claude Code sign-in and short CLI (refresh, `auth status`, `/usage`, warm-up) landed in this slice (`wslrun.rs` / `launcher.rs`; running distros only). Remaining **R-6**: per-login Bun proxy, opened-URL env, and MCP bridge WSL (`upstream.rs` `wsl_distro: None`) — **M**

---

## Cursor (`cursor`)

**Reference files read:** `internal/provider/cursor.go`, `cursor_models.go`, `migrate_side.go`; `packages/cursor/index.mjs`, `native-session.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/cursor/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: browser PKCE | Plugin: `loginDeepControl` + poll (`cursor/index.mjs:6-8`, `386+`) | `sign_in.rs` + `SignInFlowKind::Pkce` (`lib.rs:32-35`, `170+`) | Partial | Parity with plugin poll/error mapping (unverified edge cases) |
| a2 | sign-in: cursor-agent reuse | Read keychain / `auth.json`, never write (`cursor/index.mjs:8-11`, `112-118`) | `cli.rs` keychain on macOS only + platform `authFile()` reads (`auth.rs`); import/discover (`discover.rs`, `import.rs`); tests `r9i_*` / `auth.rs` | Done | — |
| a3 | sign-in: API key → token | `CURSOR_API_KEY` → `/auth/exchange_user_api_key` (`cursor/index.mjs:347-360`) | R-8c `exchange.rs` exchanges the environment or pasted key, stores only the key, caches the derived token near expiry, and refreshes it without persisting the access token | Done | — |
| a4 | sign-in: device code | N/A | N/A | N/A | — |
| a5 | import paste | Core mover reads cli-config (`cursor.go:65-80`) | `import_accounts` JSON blob | Partial | — |
| b | import sources | Agent home `.cursor` auth + keychain | `AgentSignIn` + discover (`lib.rs:37-41`); R-8d reads the matching team id from cli-config for billing | Partial | Team/active org metadata beyond the billing header |
| c | token storage | Plugin session store; core mover side logins | Daemon secret blob + optional agent read-only | Partial | Persist team/active org metadata if needed for quota |
| d | refresh | `cursor-agent status` / token TTL (`cursor/index.mjs:182-194`, CLI_MARK); one in-flight `status` (`cliRefresh`) | `refresh.rs` coalesced `StatusCoalescer` with per-account lease; API-key accounts re-exchange through R-8c; `status_coalesce_tests` | Done | — |
| e | upstream wire | Connect bidi `AgentService/Run` + HTTP/1 RunSSE + BidiAppend (`cursor/index.mjs:13-17`, `44-46`) | `run.rs` incrementally reads either transport, answers KV/exec and selected field-36 tool lists, sends heartbeats, and enforces monotonic idle/quiet/tool-count limits; `upstream.rs` opens one stateless Run per execute, retries regional and Max Mode failures as documented; R-9e maps images and R-9f sends picker-selected effort, fast, and context parameters through the same cached list as R-9g `list_models` | Partial | Native session retention remains deferred |
| f | inbound shapes | OpenAI-compatible chat to plugin fetch (`cursor/index.mjs:29-30`) | Canonical IR text, tool history, images, effort, and service tier map through `request.rs` and `picker.rs` to transcript blobs, the protobuf user turn, and native requested-model parameters; gateway codecs supply the IR | Partial | Real-account protocol verification is outside this audit |
| g | streaming | Connect stream / RunSSE (`cursor/index.mjs:13-17`) | `Completion::Stream` emits text, separate thinking, one full-argument `ToolArgumentsDelta` per selected MCP exec (the wire delivers arguments in one exec, not token deltas), clean endings, and field-14 or dashboard tool-step usage; R-9b reasoning replay and R-9c usage mapping are landed; R-9h fixture tests lock the single-shot argument shape | Done | — |
| h | tool calls | MCP `CallDynamicTool` (`cursor/index.mjs:21-26`, `51`) | Selected MCP execs emit caller tool blocks; unknown and Cursor-owned execs (shell, file, and every non-MCP kind) stay refused on the stateless Run path; server-completed calls are not emitted; counts, checkpoints, and the 60-second count wait seal steps; R-9h loopback tests cover refusal without native session | Partial | Cursor-owned tool execution remains deferred without native session retention |
| i | images | unverified in core; plugin native session handles blobs | R-9e maps data-URL and base64 images to hex `Uint8Array` parts, URL images to placeholders, non-image files to named placeholders, and sets request field 19 only for sent inline images | Done | — |
| j | reasoning | Plugin tests `reasoning.test.mjs`; native picker effort parameters | R-9b emits and replays separate thinking blocks; R-9f resolves `GenerationParams::effort` through the picker and sends the selected variant's native parameters and Max Mode | Done | — |
| k | quota / usage | Connect `GetCurrentPeriodUsage`, team header (`cursor/index.mjs:171-176`, `connect.rs` parity) | `quota()` via Connect unary with R-8d team header; R-9c maps field-14 and tool-step dashboard usage | Partial | Cross-step deferred usage and credit events (`cursor/deferred-usage.test.mjs`) |
| l | model list | Plugin catalog + agent list; core `cursor_models.go` contexts | R-9g merges picker, `GetUsableModels`, and scope-3 `AvailableModels` with `fromCatalog` / `modelsOf`, caches by SHA-256(token) for ten minutes, coalesces in-flight listing, and serves stale-while-revalidate / `LIST_RETRY` on the request path (`listing.rs`, `catalog_tests.rs`; recorded HTTP only) | Done | — |
| m | multi-account | Team id per token (`activeTeamId`) | Multiple accounts; R-8d sends the matching `x-cursor-team-id` on chat, dashboard usage, quota, and picker calls | Partial | — |
| n | error classification | Plugin maps budget/refusal (`errors.test.mjs`) | `failure.rs` implements the text-only section 6 table: regional/permission 403, status-style 401, quota vs rate 429, context overflow and invalid input 400, unavailable 503, other Connect codes 502, and unchanged bare HTTP statuses. R-9d retries once on a `maxRequired` refusal when the first attempt was not Max Mode | Partial | — |
| o | warmup / keepalive | none found (no keepalive in the Cursor core or its auth plugin) | None | N/A | — |
| p | agent sign-in | Core `cursorStatus` mark (`cursor.go:59-80`) | discover/import (`lib.rs:383-396`) | Done | — |
| q | extras | HTTP/2 fallback, ghost mode, client version string (`cursor/index.mjs:48`, `connect.rs:24-26`) | Run headers and HTTP/2 transport plus RunSSE/BidiAppend fallback in `run.rs`; R-8d production execute resolves and forwards the optional team id, with loopback tests | Partial | — |

### Gaps (Cursor)

- Cross-step deferred usage and credit events remain in row k; Cursor-owned tool execution without native session retention remains in row h.

---

## Devin (`devin`)

**Reference files read:** `internal/provider/devin.go`, `devin_accounts.go`, `devin_images.go`; `packages/devin/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/devin/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: CLI PKCE | Plugin: loopback PKCE via `app.devin.ai` (`devin/index.mjs:28-31`, `SIGN_IN_TIMEOUT`) | `sign_in.rs` + loopback PKCE, fixed-server `ExchangeDevinCLIPKCECode`, and GetUserStatus identity; offline callback/exchange tests | Done | — |
| a2 | sign-in: reuse CLI | Read `credentials.toml` (`devin.go:54-67`) | `creds.rs` + discover; CLI-sourced accounts re-read their imported TOML under a 30 s lease on execute; browser accounts do not | Done | — |
| a3 | API key / device | N/A | N/A | N/A | — |
| b | import | Side login mover + CLI path | XDG/agent `credentials.toml` discover/import with identity labels and string `cli = "true"` source metadata | Done | — |
| c | token storage | Plugin + TOML on disk | Daemon secret; atomic agent write on switch | Partial | — |
| d | refresh | Session tokens do not expire; CLI-sourced accounts re-read `credentials.toml` on refresh and on the request path (`index.mjs:2519-2523`, `3075-3088`) | `Stored::live` on `refresh` and `list_models`; execute already re-reads under the account lease (`creds.rs`, `lib.rs`) | Partial | No OAuth refresh-token grant—the reference stores `refresh: ""` and `expires: 0` |
| e | upstream wire | Protobuf Connect `GetChatMessage` on `server.codeium.com` (`devin/index.mjs:28-32`) | `upstream.rs` unary POST with literal Basic auth, 300 s read budget, origin override, and tool field 10 | Done | — |
| f | inbound shapes | Chat completions + Anthropic for Claude models (`devin/index.mjs:19-26`) | Chat, Responses, and Messages IR execute; R-13c preserves Messages block boundaries and field-8 cache marks; the catalog advertises `/v1/messages` for Claude ids and `/v1/chat/completions` for every other model (`models.rs`) | Partial | Real-account protocol verification is outside this audit |
| g | streaming | Connect stream from GetChatMessage | Incremental text, reasoning, tool-start, and argument-delta IR events; content-or-tool-gated success | Done | — |
| h | tool calls | Plugin `tool-ids.test.mjs`, split calls | R-13b encodes field-6 calls and field-7 results, joins split calls, fills interrupted results, sanitizes ids, offers history-only tools, and streams tool-only replies with usage | Done | — |
| i | images | Plugin `images.test.mjs`, `devin_images.go` in core catalog | R-13b encodes inline field-10 images, omits URL-only images, and keeps direct or `FROM`/`NOTE`-associated tool-result images on their tool prompts | Done | — |
| j | reasoning / thinking | `thinking-signature.test.mjs`, effort variants | R-13b replays unsigned reasoning on non-Claude uids, signed Claude thinking on fields 11/12/18, and fragmented Claude signatures from fields 10/21 with a 512-entry type map | Done | — |
| k | quota / usage | Plugin `usage.test.mjs` | R-13a `quota.rs` reads daily/weekly and informational ACU windows; free ids survive as model exclusions; field 7 becomes finish usage with cache counters and output estimation | Partial | Plan/balance presentation and organization metadata are not represented by `Subscription::quota` |
| l | model list | CLI JSON + `GetCliModelConfigs` (`devin/index.mjs:44-134`) | Live raw-protobuf RPC, isolated CLI fallback, embedded snapshot, per-account freshness/retry cache, tier-4 free flags, and matching-variant list prices | Done | — |
| m | multi-account | EU host `eu.codeium.com` (plugin) | unverified in hofund | Partial | Region selection (**R-12**) |
| n | error classification | Plugin `errors.test.mjs` | `errors.rs` section 6 HTTP/Connect mapping, exact empty-reply and broken-stream errors | Done | Tool/image-specific cases (**R-13**) |
| o | keepalive | `keepalive.test.mjs` | R-13c adds opt-in 285 s / six-ping keepalive with byte-stable `continue` requests, send-time scheduling, session/subagent and growing-first-message keys, and all stop rules; paused-clock recorded-HTTP tests | Partial | Separate ping usage reporting needs a host usage hook |
| p | agent sign-in | Core installs CLI (`devin.go:8-12`) | `agent_sign_in.rs` writes TOML; R-13c discovers PATH/home/system binaries, runs the reference installer, rechecks discovery, refreshes registry PATH, and applies the Windows setup tail with scripted-process tests | Done | — |
| q | extras | gzip request, identity headers, CLI version pin (`devin/index.mjs:33-36`) | `connect/` message tables + SDK `protobuf`; 8083/8333 gzip tests; HTTP encoding header, metadata, and sensitive Basic auth | Done | — |

### Gaps (Devin)

- Keepalive usage reporting — host usage hook (deferred by design)

Structured-output prose for Chat, Responses, and Messages shipped in R-13b (`structured-output.test.mjs` behavior; `structured_text` in `upstream.rs`).

---

## Zed (`zed`)

**Reference files read:** `internal/provider/zed.go`, `zed_body.go`, `migrate_zed.go`; `packages/zed/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/zed/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: native app RSA loopback | Plugin opens `zed.dev/native_app_signin` (`zed/index.mjs:8-13`) | `sign_in.rs` + kernel query-token loopback; RSA-2048, OAEP-SHA256 + v1.5 fallback, 10-minute driver deadline; `r56_*` tests with loopback and mock `/client/users/me` | Done | Mocks only; no real-account smoke |
| a2 | sign-in: import saved creds | Core `zedCreds` mover (`zed.go:14-34`) | `import.rs` remains alongside `Pkce` loopback flow | Partial | — |
| a3 | API key / device | N/A | N/A | N/A | — |
| b | import | Built-in side logins | `hofund subscription import` | Partial | — |
| c | token storage | Plugin holds user id + access + system id | `creds.rs` stored secret | Partial | — |
| d | refresh | POST `/client/llm_tokens` on 401 (`zed/index.mjs:14-16`) | `refresh.rs`, `tokens.rs`, stale headers on `/models` and `/completions` | Done | — |
| e | upstream wire | POST `/completions` NDJSON envelope (`zed/index.mjs:17-20`) | `upstream.rs` POST `/completions` | Done | — |
| f | inbound shapes | Per-provider SDK body in `provider_request` (`zed/index.mjs:22-25`) | `body.rs` + `wire::encode_outbound` | Done | — |
| g | streaming | NDJSON `event` lines | `stream.rs` → wire decode; unary fold | Done | — |
| h | tool calls | Provider-native in envelope | End-to-end via encoded provider bodies | Done | — |
| i | images | Model flags `supports_images` (`zed/index.mjs:57-62`) | `prepare_provider` strips images when disabled | Done | — |
| j | thinking | `supports_thinking` in catalog | Anthropic/Gemini/OpenAI body shaping | Done | — |
| k | quota / usage | Plan limits via Zed cloud (plugin); core reader minimal | `quota.rs` plan window from `/client/users/me` | Partial | No allowance meters (reference has none) |
| l | model list | GET `/models` (`zed/index.mjs:17-18`) | Live fetch with snapshot fallback (`models.rs`) | Done | — |
| m | multi-account | Org id on creds (`zed.go:21-22`) | Org on token mint, repick on 402 or empty list, persisted via `apply_org` | Partial | No org picker UI |
| n | error classification | `SignInExpired`, 402 plan (`zed/index.mjs:83-147`) | `errors.rs` auth/quota/status mapping | Done | — |
| o | warmup | N/A | N/A | N/A | — |
| p | agent sign-in | N/A (Zed editor sign-in, not agent file) | No agent discover | N/A | — |
| q | extras | User-Agent `Zed/1.23.0` (`zed/index.mjs:35-69`) | `headers.rs` on client and model calls | Done | — |

### Gaps (Zed)

- Org picker UI for multi-account (stored org + automatic repick only)
- Plan-only quota windows (no allowance meters; matches reference)
- Gateway-facing Zed client API shapes beyond subscription upstream (parity doc only; not in R-16 scope)

---

## Kiro (`kiro`)

**Reference files read:** `internal/provider/kiro.go`, `kiro_accounts.go`, `migrate_kiro.go`; `packages/kiro/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/kiro/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund R-51c | status | gap to close |
|---|------------|------------------------------|--------------|--------|--------------|
| a1 | sign-in: remote portal (Google/GitHub/Builder ID/IDC) | Plugin loopback ports + AWS OIDC (`kiro/index.mjs:3-8`, `37-38`) | `portal.rs` binds the documented ports, exchanges social codes, and runs register/authorize/token for Builder ID and enterprise; pasted callbacks forward through the daemon | Done | — |
| a2 | sign-in: reuse kiro-cli / IDE | `readCLI` / `readIDE` (`kiro/index.mjs:117-181`); core `kiro.go:19-22` | `site=current` reads the CLI DB first, then the IDE token file | Done | — |
| a3 | sign-in: API key `ksk_` | Plugin + core treat as API key | unverified — method `apikey` in upstream headers (`upstream.rs:54-55`) | Partial | Explicit API key sign-in UX |
| b | import | IDE cache file; CLI DB | `cli_db.rs` reads `auth_kv` social, odic, and external-idp rows; `discover.rs` keeps IDE import | Done | — |
| c | token storage | Write-back to CLI DB or IDE file on refresh (`kiro/index.mjs:183-200`) | Source metadata selects compare-and-swap SQLite or IDE JSON write-back after refresh | Done | — |
| d | refresh | CLI first, else OIDC/refreshToken (`kiro.go:301-374`; plugin `refresh.test.mjs`) | `refresh.rs` delegates `debug refresh-auth-token`, re-reads the CLI DB, and supports social, IDC, and external-idp form refresh | Partial | Refresh 400 lapsed mapping (**R-24**) |
| e | upstream wire | Event-stream `generateAssistantResponse` (`kiro/index.mjs:16-18`; `upstream.rs:44-48`) | Same endpoint and headers; `open_raw` feeds unary and streaming decoders | Partial | Mock/loopback verification only (**R-51**) |
| f | inbound shapes | Anthropic Messages only (`kiro/index.mjs:14-18`) | Messages only; Chat/Responses are not invented | N/A | — |
| g | streaming | AWS event stream to Messages SSE | `kiro_stream.rs` incrementally emits IR text, reasoning and signature deltas, tool starts, argument deltas, block stops, and finish events | Partial | Mock/loopback verification only (**R-51**) |
| h | tool calls | Plugin tool tests (`tool-names.test.mjs`) | `kiro_body.rs` maps declarations, normalized ids/names, split calls, and matched/missing results; replies restore caller names | Partial | Mock/loopback verification only (**R-51**) |
| i | images | `image-size.test.mjs` | Inline user/tool-result images keep the latest-image rule. Over-5-MiB Base64 images follow the reference's same-format/lossy, quality, and edge sequence with a four-entry cache; failures use its exact note (`r51c_*`, deterministic fixtures) | Done | — |
| j | reasoning / effort | `effort.test.mjs` | Enabled/adaptive thinking uses native `additionalModelRequestFields` when the listed schema allows the requested effort (`output_config`, then `reasoning`), otherwise the reference budget tags. `reasoningContentEvent` text and signatures fold into unary blocks and stream as reasoning/signature deltas; structured output stays in instructions (`r51c_*`) | Done | — |
| k | quota / usage | `Get-Usage-Limits` (`kiro.go` comments; plugin `usage.test.mjs`) | `quota.rs` with profile ARN and Builder ID default | Partial | Full allowance/bonus window parity |
| l | model list | Plugin lists from API | `models.rs` fetches the profile-scoped list, pages the API-key control-plane list, and keeps default order, limits, image flags, effort lists, and native schemas per account/model | Partial | Recorded model-list verification only (**R-51**) |
| m | multi-account | uid by login provider (`kiro/index.mjs:53-65`) | Browser ids include label/provider; imported ids remain agent-scoped | Partial | Stable ids (#607) (**R-24**) |
| n | error classification | Plugin `errors.test.mjs`; refresh 400 → lapsed | Generic auth on 401/403 (`upstream.rs:60-63`) | Partial | **R-24** |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | IDE cache and CLI DB discover | `discover.rs` prefers CLI DB, then IDE; `agent_sign_in.rs` keeps IDE switching | Done | — |
| q | extras | Builder ID profile ARN constant (`kiro/index.mjs:50-51`) | `BUILDER_ID_PROFILE` applies to imported and browser Builder ID sign-ins | Done | — |

### Gaps (Kiro)

- Live-account verification — **M**, **R-51**
- Refresh 400 lapsed + stable account ids — **S**, **R-24**

---

## Factory / Droid (`factory`)

**Reference files read:** `internal/provider/factory.go`, `factory_key.go`, `factory_usage.go`, `migrate_factory.go`; `packages/factory/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/factory/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund R-52 | status | gap to close |
|---|------------|------------------------------|-------------|--------|--------------|
| a1 | sign-in: WorkOS device | Plugin WorkOS device + production `CLIENT_ID` (`factory/index.mjs:12-16`) | `SignInFlowKind::Device`; the reference production client is used on device start, polling, and refresh (`lib.rs`, `sign_in.rs`, `tokens.rs`; `a2b_12`, `a2b_13`) | Done | Recorded HTTP only |
| a2 | sign-in: API key `fk-` | `factoryCreds.Key` (`factory.go:68-71`); plugin whoami | `import_api_key` keeps the key, region, and prem host, without an OAuth org header or refresh (`sign_in.rs`; `a2b_18`) | Done | — |
| a3 | browser OAuth loopback | N/A (device) | N/A | N/A | — |
| b | import | Mover reads built-in accounts | `discover.rs` lists and `import_accounts` stores mover OAuth and API shapes plus built-in `factoryCreds` in `logins.json` without live whoami (`r52d_*`) | Done | — |
| c | token storage | JSON creds with org/region/prem (`factory.go:49-72`) | Secret blob stores org, region, and whoami's `premBaseHostV2`; refresh reconciliation preserves and updates it (`sign_in.rs`, `tokens.rs`) | Done | — |
| d | refresh | WorkOS refresh; API key no refresh; two-minute request lead, three-minute host lead, request-time unexpired-token grace, and strict explicit transient failures (`refresh.test.mjs`) | Two-minute request lead, lease serialization, request-time-only unexpired-token grace, explicit transient-failure propagation, non-429 4xx lapse, and whoami reconciliation (`tokens.rs`; `a2b_13`, `a2b_17`, `r52_transient_*`) | Partial | The host API has no separate three-minute `refreshLead` setting |
| e | upstream wire | Per-model `/api/llm/{a,o,g}/v1`; prem host override (`factory/index.mjs:33-39`, `factory.go:65-67`) | `wire_target` plus canonical HTTPS prem-host routing; invalid hosts fail before HTTP (`upstream.rs`; `r52_prem_*`) | Done | Recorded HTTP only |
| f | inbound shapes | Anthropic, Responses, Chat, Gemini; `fastBody` and multipart `droidChat` (`factory/index.mjs:128-158`, `:583-606`) | Four wires, six reference Fast Mode models, base-model speed/priority rewriting, fast beta, joined all-text Chat system content, and a separate Droid opening for mixed content (`models.rs`, `upstream.rs`; `r52_fast_*`, `r52_chat_system_*`) | Done | Recorded HTTP only |
| g | streaming | Via AI SDK wire helpers; Gemini `/generate` | SDK streaming plus Gemini `/generate`, with model in the body (`wire.rs`, `upstream.rs`; `r52_gemini_stream_*`) | Done | Recorded HTTP only |
| h | tool calls | `anthropic-tools.mjs` wraps root-composed schemas and unwraps unary/streamed calls | `anthropic_tools.rs` prepares wrapped schemas before encode and unwraps unary and streamed Anthropic replies through wire adapters (`upstream.rs`; `r52e_*`) | Done | Recorded HTTP and deterministic fixtures only |
| i | images | Model table `images` flag (`factory/index.mjs:59-116`) | Every catalog entry carries image and modality flags (`models.rs`) | Done | Recorded fixtures only |
| j | reasoning | Effort variants and Gemini `thinkingConfig` (`effort.test.mjs`, `gemini.test.mjs`) | Reference effort lists and preserved Gemini `thinkingConfig` (`models.rs`; `r52_gemini_*`) | Done | Recorded fixtures only |
| k | quota / usage | GET `/api/billing/limits`; Standard/Core scopes, extra balance, and optional `standard_only` (`usage.test.mjs`) | Standard excludes Core, Core has its model list, and extra usage is non-blocking display text (`quota.rs`, plugin API/router propagation; `r52_core_*`, `a2b_38`) | Partial | The `standard_only` switch and its alternate routing mode remain |
| l | model list | Static MODELS + promotions and `rateWas` (`rate.test.mjs`) | Full ordered table, fixed-window promotion rates, and pre-promotion multipliers exposed through model state and control responses (`models.rs`, `models_state.rs`, `control.rs`; `r52_promotion_*`) | Done | Recorded fixtures only |
| m | multi-account | Org active id header and EU region base (`factory.go:57-79`) | Org header plus `api.eu.factory.ai` for EU accounts (`upstream.rs`; `a2b_40`, `a2b_42`) | Done | Recorded HTTP only |
| n | error classification | Lapsed refresh, content policy, and rate rests (`lapsed.test.mjs`, `content-policy.test.mjs`) | OAuth-only 401 renewal; non-429 4xx refresh lapse; policy 403 becomes non-cooling 400 without the upstream-error header; 429 keeps retry delay (`tokens.rs`, `upstream.rs`; `r52_content_policy_*`, `r52_rate_limit_*`) | Done | Recorded HTTP only |
| o | warmup | N/A | N/A | N/A | — |
| p | agent sign-in | N/A | N/A | N/A | — |
| q | extras | `X-Client-Version` 0.242.0 (`factory/index.mjs:17-23`) | `client.rs` pins 0.242.0 and `cli` / `factory-cli` identities on API and model calls (`r52_prem_*`) | Done | Recorded HTTP only |

### R-52 completed

- Fast Mode uses the six reference ids, rewrites base-model Anthropic `speed: fast` and Responses `service_tier: priority`, and adds the fast beta on Anthropic.
- Whoami's `premBaseHostV2` is stored on sign-in and refresh; model calls normalize and use it, while invalid or non-HTTPS hosts fail closed. Quota remains on the regional API.
- The model table includes the reference's current entries and Fast Mode entries last; active promotions set both `rate` and `rateWas` with start-inclusive/end-exclusive windows.
- Device sign-in and refresh use the reference's production WorkOS client. Gemini streams use `/generate`; content-policy and rate-limit handling have recorded-request coverage.
- Anthropic tool schemas with root unions ride under `arguments`, rebase in-schema `#` refs, hide the envelope in unary replies and streamed `input_json_delta` events, and keep wrapped history on the wire (`anthropic_tools.rs`; `r52e_*`).
- All added tests use deterministic fixtures or recorded HTTP. No live-account verification was performed.

### Remaining gaps (Factory)

- The host-level three-minute refresh setting and `standard_only` remain Partial as listed above.

---

## Cline / ClinePass (`cline`)

**Reference files read:** `internal/provider/cline.go`, `cline_usage.go`; `packages/cline/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/cline/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund R-53 | status | gap to close |
|---|------------|------------------------------|-------------|--------|--------------|
| a1 | sign-in: WorkOS device | Plugin device + register (`cline/index.mjs:3-19`) | `DeviceFlow` + same `WORKOS_CLIENT_ID`, then `/auth/register` (`sign_in.rs`, `a2a_17`) | Done | — |
| a2 | sign-in: API key | Plugin key auth | `SignInProof` API key path in `sign_in.rs` | Done | — |
| a3 | browser loopback | N/A | N/A | N/A | — |
| b | import | Preset/key plan in core | OAuth/API key via sign-in only | Partial | — |
| c | token storage | Plugin session; core uses key | Secret blob + `workos:` prefix handling; 30-second request-time retryable grace in `tokens.rs` | Done | — |
| d | refresh | WorkOS refresh + grace (`cline/index.mjs:25-30`, `883-895`) | Ten-minute hook lead, five-minute request lead, request-only transient grace, and lease-serialized single-use rotation (`tokens.rs`, `r53_*`) | Done | — |
| e | upstream wire | OpenAI chat to `api.cline.bot` (`cline.go:43-49`; plugin) | `upstream.rs` OPENAI_CHAT + envelope unwrap | Partial | Recorded mocks only |
| f | inbound shapes | Chat completions + pinned CLI headers (`cline/index.mjs:38-74`) | `client.rs`: `cline-cli` 3.0.68, platform `cli`, core 0.0.90 on every API call | Done | — |
| g | streaming | Plugin stream chat | `wire::execute` | Partial | unverified parity |
| h | tool calls | Through chat API | wire layer | Partial | — |
| i | images | Catalog `input.image` per model and image-part handling (`packages/cline/index.mjs:356-357`, `482-518`) | `models.rs` carries exact/alias-agreed input modalities and image flags; OpenAI Chat encodes inline images (`r53_*`) | Done | — |
| j | reasoning | Model records set `reasoning: false` | Empty effort lists; no reasoning events or budgets invented | Done | — |
| k | quota / usage | GET `/users/me/plan/usage-limits` + credits (`cline_usage.go:39-67`) | `quota.rs`, including credits-only fallback only on a limits 404 | Done | — |
| l | model list | Required `recommended-models` groups plus optional additive `/ai/cline/models` | `models.rs` merges all three groups with cloud context and input capabilities; cloud failure keeps the required feed | Done | — |
| m | multi-account | Multiple keys/accounts | Daemon multi-account | Done | — |
| n | error classification | NeedsSignIn on hard expire (`cline/index.mjs:27-30`) | Refused refresh sets lapsed / NeedsSignIn; transient errors do not | Done | — |
| o | check-in | N/A | N/A | N/A | — |
| p | agent sign-in discovery | N/A (agent uses provider JSON) | No discover (`subscriptions.md:100`) | N/A | — |
| q | extras | Task id header (`cline/index.mjs:62-75`) | `client.rs` derives the reference SHA-256/UUID-layout id from host sessions and omits empty/synthetic sessions | Done | — |

### R-53 completed (Cline)

- Pinned CLI client identity on every API request, including platform and core headers.
- Host-session task ids, with no header for empty or synthetic sessions.
- Required recommended plus optional additive cloud catalogs, image capabilities, and OpenAI image parts.
- Ten-minute hook renewal, five-minute request refresh, single-use leased rotation, and request-only transient grace.
- Confirmed `reasoning: false`; no events, efforts, or budgets were added.

Remaining verification is live-account acceptance; all evidence here is deterministic fixtures or recorded HTTP.

---

## ChatGPT / Codex (`codex` backend) and ChatGPT API (`chatgpt-api`)

**Reference files read:** `internal/provider/signin.go`, `codex_store.go`, `import_codex.go`, `codex_request.go`, `codex_models.go`, `codex_limits.go`, `codex_credits.go`, `codex_resets.go`, `codex_zstd.go`, `codex_http1.go`, `codex_device_signin.go`, `codex_switch.go`, `warmup_daily.go`, `chatgpt_api.go`, `internal/gateway/codex_backend.go`.

**hofund path:** `plugins/subscriptions/codex/` (`lib.rs`, `sign_in.rs`, `device_sign_in.rs`, `tokens.rs`, `discover.rs`, `agent_sign_in.rs`, `upstream.rs`, `limits.rs`, `usage.rs`, `resets.rs`, `models.rs`, `warmup.rs`, `shape.rs`). **ChatGPT API:** `plugins/subscriptions/chatgpt-api/` (`lib.rs`, `sign_in.rs`, `host_id.rs`, `tokens.rs`, `revoke.rs`, `models.rs`, `shape.rs`, `errors.rs`, `upstream.rs`, `quota.rs`).

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: browser / loopback OAuth (Codex CLI PKCE) | `signin.go:38-45` codex authorize URL, fixed `127.0.0.1:1455`, scopes; loopback callback | `codex/src/lib.rs:191-241`, `sign_in.rs:16-37` same client, port 1455, PKCE + loopback | Done | — |
| a2 | sign-in: device code | `codex_device_signin.go` (device auth APIs) | `device_sign_in.rs:27-30`, `lib.rs:196-206` `--device-auth` flow | Done | — |
| a3 | sign-in: API key (detect only) | Codex `auth.json` with `auth_mode: apikey` is not an account: discovery and agent switch drop it (`logins.go:767` `codexLoginIn`, `account.go:1044` `codexAccount`); `CodexAPIKeySignedIn` (`logins.go:571`) also lets `/responses/compact` pass a key's account holds (`gateway/codex_backend.go:693` `compactSigninHeld`) | `discover.rs:73-80` drops `auth_mode: apikey` on read/list/import; `agent_sign_in.rs:47` writes `auth_mode: chatgpt`; `codex-catalog/src/compact_hold.rs` mirrors `compactSigninHeld` on `/backend-api/codex/responses/compact`; tests `r47a_a3_*` | Partial | API-key mode is still detect-only on the subscription path (not a stored account); compaction allowance is on the inbound backend only |
| a4 | sign-in: paste callback | `signin.go:69` PasteCallback on sign-in state; the pasted browser-final address goes through the same checked code exchange | `lib.rs` pending browser state + existing `SignInProof::Callback`; daemon prompt and loopback parser accept the pasted address, with wrong host/port/path/state refused; `r47c_codex_sign_in_completes_from_pasted_callback` proves the real plugin exchanges the code and stores the account | Done | — |
| a5 | sign-in: ChatGPT API (Sign in with ChatGPT / dynamic client) | `chatgpt_api.go:42-66` `dynamic_agent_client`, per-account client id, `siwcAuthorize` | `chatgpt-api/src/sign_in.rs`, `host_id.rs`, loopback `127.0.0.1:1455` + IPv4 redirect; `SignInProof::Callback::issued_client_id`; tests with recorded token exchange | Partial | Live OpenAI registration semantics; no Codex-catalog model union (`siwcWithCodex`) |
| b | import sources | `codex_store.go:517+` read/write Codex `auth.json` and macOS Keychain; `import_codex.go` config.toml providers | `discover.rs:19-54` read `auth.json` only; `agent_sign_in.rs:9-71` write `auth.json` (file mode) | Partial | Keychain read/write, config.toml provider import (R-47) |
| c | token storage / encryption | `logins.json` + optional Codex home/keychain (`codex_store.go`) | daemon `secret_ref` encrypted store via `host.accounts()` (`crates/kernel/daemon/src/subscriptions.rs` pattern) | Partial | Bidirectional Codex store sync with Keychain (R-47) |
| d | refresh | OAuth refresh with rotation; Codex/keychain writers (`codex_store.go` write) | `tokens.rs:78-176` lease, `LAPSED` on refused refresh, 5 min margin | Partial | Persist refresh back to Codex Keychain when Codex rotates alone (R-47) |
| e | upstream wire | POST `https://chatgpt.com/backend-api/codex/responses`, zstd bodies (`codex_zstd.go:12-24`), HTTP/1.1 fallback (`codex_http1.go`) | `plugin-sdk` `codex_zstd.rs` + `wire.rs` zstd level 3 on POST `{base}/responses`; `upstream.rs` `chatgpt_codex_zstd(true)`; plugin HTTP uses the host unary pool built without reqwest `http2` (`crates/kernel/plugin-host/Cargo.toml:21`, test `pool.rs` `pooled_client_sends_an_http11_request_line`); a later Connect duplex client must pin unary dashboard calls to HTTP/1.1 (Cursor Connect design, row **e** above); tests `r47a_e_*`, SDK `r47a_e_*` | Done | — |
| f | inbound API shapes | Gateway `codex_backend.go` serves Codex path; pool accounts use Responses upstream | Plugin upstream `ProtocolId::OPENAI_RESPONSES` only (`upstream.rs:84`); gateway codec translates Chat/Responses/Messages/Gemini to IR | Partial | Codex-as-provider gateway mirror (`codex_backend.go`) not in hofund scope here; subscription path is Responses-only |
| g | streaming | SSE stream; fold for unary (`codex_request.go` patterns) | `upstream.rs:96-120` forces stream, fold when unary | Done | — |
| h | tool calls | Codex tool name mapping in gateway/provider | `upstream.rs:82-94` `codex_tool_names` restore | Done | — |
| i | images | via Responses content parts (gateway codex) | `tests.rs` `r47a_i_*`: Responses `input_image` in upstream POST body | Done | — |
| j | reasoning / thinking | Codex model instructions / reasoning items in gateway; native compaction items and triggers pass through; content-bearing or unsealed reasoning is dropped before replay; unreadable sealed items removed and the turn retried on 400/404 | `shape.rs` keeps only sealed reasoning without content parts (with ids on sealed replay), restores codec-carried native compaction items, and preserves triggers; `refusal.rs` and `upstream.rs` trim the posted Responses JSON after unreadable-item refusals up to three tries; recorded fixtures `r47c_*` and `r47j_*` | Done | — |
| k | quota / usage | Headers + `codex.rate_limits` event; `/wham/usage`; credit resets (`codex_resets.go:180+`) | `limits.rs`, `usage.rs` headers, event, `/wham/usage`; `resets.rs` + `Subscription::quota_reset_credits` / `consume_quota_reset` on `/wham/rate-limit-reset-credits` (consume only on explicit calls); tests `r47a_k_*` | Partial | UI cards and daemon CLI for reset consume (R-47 later slice) |
| l | model list | `codex_models.go` backend list + instructions | `models.rs` `/codex/models`, cached prompts | Done | — |
| m | multi-account | Multiple logins; routing in provider | Multiple accounts via daemon subscription members | Done | — |
| n | error classification | Expired → refresh; spent windows; credits (`codex_credits.go`); gateway remote reset and week/expiring autoreset (`codex_autoreset.go`, `codex_expiring.go`, `codex_resets_used.go`) | `tokens.rs` lapsed; `limits.rs` observe blocking vs credits; `resets.rs` vendor refusal outcomes on HTTP 200 and in-flight `redeem_request_id` reuse for explicit `consume_quota_reset` only (`r47n_*`) | Partial | Automatic consume on 429 or expiring windows, `CodexAutoReset` settings, reset-lead tuning, remote-gateway `POST /v1/.../codex-reset`, and resets-used history cards remain unported by policy |
| o | daily warmup | `warmup_daily.go` + provider warmup | `lib.rs:372-377` `daily_warmup` → `warmup.rs` | Done | — |
| p | agent sign-in switch | Writes Codex auth (`codex_switch.go`, store) | `write_agent_sign_in` → `agent_sign_in.rs` | Partial | Keychain mode + lock interaction with live Codex unverified |
| q | extras | `chatgpt-account-id`, Codex version headers, plan in ID token | `upstream.rs:172+` headers, `CODEX_VERSION`, `sign_in.rs:13` auth claim; zstd on Codex turns (row **e**) | Done | — |

**Gaps**

- Finish `chatgpt-api` parity: model-list union with Codex catalog when a ChatGPT plan account shares models, and live-account verification — **M**, **R-47** follow-up.
- Codex Keychain + two-writer auth.json sync — **M**, **R-47**.
- Credit reset consume surfaced in CLI/control UI cards — **M**, **R-47** (plugin API and Codex `/wham/rate-limit-reset-credits` read/consume are in place; automatic consume is forbidden).
- Codex week/expiring autoreset, auto-reset settings, remote-gateway reset route, and reset-use history — **L**, out of scope for the subscription plugin (R-47n documents the honest remainder; explicit consume only).


---

## GitHub Copilot (github.com and GHE)

**Reference files read:** `copilot_signin.go`, `copilot_accounts.go`, `copilot_cli.go`, `copilot_entitlement.go`, `copilot_ghe.go`, `copilot_lapsed.go`, `copilot_refused.go`, `side_logins.go`, `internal/gateway` (Copilot routes via provider session endpoints).

**Community plugin (parity target for retired path):** `packages/copilot/index.mjs`, `endpoints.mjs`, `usage.mjs`.

**hofund path:** `plugins/subscriptions/copilot/`.

| # | capability | reference v0.1.3005 | hofund R-48 | status | gap to close |
|---|------------|----------------------|-------------|--------|--------------|
| a1 | sign-in: device flow | `copilot_signin.go:31-38`, `59-86` GitHub device OAuth app (client id in `copilot_signin.go`) | `sign_in.rs`, `lib.rs:138-150` device flow | Done | — |
| a2 | sign-in: browser OAuth | N/A (device only for reference-added accounts) | N/A | N/A | — |
| a3 | sign-in: API key | N/A | N/A | N/A | — |
| a4 | sign-in: paste | N/A | N/A | N/A | — |
| a5 | sign-in: editor / CLI login reuse | `copilot_accounts.go:3-8`, `copilot_cli.go:81+` read editors’/CLI OAuth, never write | `discover.rs`, `session.rs`; editor `apps.json`/`hosts.json`, CLI JSONC and keychain reads; direct CLI sessions; read-only stale/recovery fixtures | Done | — |
| a6 | sign-in: GHE host option | `copilot_ghe.go`, `CopilotHost` in sign-in | `site.rs:9-40`, `lib.rs:72-81` `host` config → `.ghe.com` URLs | Done | — |
| b | import sources | Side login from editor token metadata (`copilot_accounts.go`) | `discover_agent_sign_in` / `import_agent_sign_in`; hofund stores only source path, host, and login, never the external token | Done | — |
| c | token storage | GitHub token in `logins.json`; Copilot session token minted per request | encrypted account secret + `TokenCache<Session>` (`session.rs`) | Done | — |
| d | refresh | GitHub token long-lived; Copilot `copilot_internal/v2/token` remint (`copilot_accounts.go` session) | `session.rs` cache with margin, 401 remint (`upstream.rs:105-114`) | Done | — |
| e | upstream wire | Session `endpoints.api` (githubcopilot.com or GHE host) JSON | `upstream.rs:81-90` bearer to session base | Done | — |
| f | inbound shapes | Chat, Responses, Messages per model endpoints | `models.rs:242-267`, `upstream.rs:54-72` protocol pick + policy accept | Done | — |
| g | streaming | Supported on chat/responses | `wire::execute` stream path | Done | — |
| h | tool calls | Copilot tool schemas | standard IR tools on wire | Partial | unverified against GHE tenant |
| i | images | Vision header when images present | `upstream.rs:137-139` `Copilot-Vision-Request` | Done | — |
| j | reasoning | model-dependent | unverified | Partial | — |
| k | quota / usage | `copilot_internal/user` plan, premium requests | `quota.rs` plan/SKU labels on cards; guarded entitlement persistence and missing-field retention in `session.rs` and `lib.rs` | Done | — |
| l | model list | From session + internal APIs | `models.rs` list + terms acceptance | Done | — |
| m | multi-account | Editor account + reference device accounts (`copilot_accounts.go`) | device/editor/CLI coexistence; host-qualified ids and labels; legacy bare GHE label migration; two-host offline quota fixtures | Done | — |
| n | error classification | `copilot_lapsed.go`, `copilot_refused.go` | session and quota 401 lapse, replacement-token guard, stale external `NeedsSignIn`, Auto/model refusal and session-loss mapping | Done | — |
| o | daily check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in switch | Editor account is read-only reference | no agent sign-in on copilot spec | N/A | — |
| q | extras | Editor version headers, initiator, API version | editor/CLI/session/model/chat versions plus Auto `2026-08-01` and `2025-10-01`; offline GHE host, naming, and base-URL fixtures | Done | Live GHE tenant verification remains unverified |

**Gaps**

- Live GHE data-residency tenant verification — not done; R-48 covers host URLs, host-qualified account names, plan/usage, and API base selection with offline fixtures only.

---

## Gemini CLI (Code Assist)

**Reference files read:** `google.go:71-100` (gemini app), `google_import.go`, `gemini_api.go`, `internal/gateway/codeassist.go` (envelope, tools, thought signatures).

**hofund path:** `plugins/subscriptions/gemini/`.

| # | capability | reference v0.1.3005 | hofund R-49d | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: loopback OAuth (Gemini CLI client) | `google.go:71-80` client id, `/oauth2callback` | `gemini/src/sign_in.rs`, `lib.rs:48-52` same client/secret | Done | — |
| a2 | device / API key / paste | N/A for Gemini CLI built-in | N/A | N/A | — |
| b | import sources | `google_import.go:178+` Cockpit/Manager/CLIProxyAPI exports; read `~/.gemini/oauth_creds.json` never write | `import.rs` reads all export shapes, nested Manager tokens, bare token lines, and project ids with app/dedup validation; `discover.rs` reads OAuth credentials, the active account, and `.env` project without writing; `r49d_*` fixtures | Done | — |
| c | token storage | `logins.json`; refresh in memory only | `tokens.rs:1-4` in-memory refresh, stored refresh unchanged | Done | — |
| d | refresh | `google.go` in-memory refresh at oauth2.googleapis.com | `tokens.rs:33-52` TokenCache + lapsed flag | Done | — |
| e | upstream wire | `cloudcode-pa.googleapis.com` `v1internal:{generateContent,streamGenerateContent?alt=sse}` envelope | `upstream.rs:17-54` same pattern, `method_root` | Done | — |
| f | inbound shapes | Gateway maps Chat/Responses/Messages/Gemini → envelope (`codeassist.go`) | Upstream `GENERATIVE_LANGUAGE`; gateway codec to IR | Done | — |
| g | streaming | SSE `streamGenerateContent` | `wire::execute` streaming | Done | — |
| h | tool calls | Gateway plain schema / function declarations (`codeassist.go`) | `code_assist.rs` keeps `parametersJsonSchema`, drops duplicate results, pairs interrupted calls, and separates response/text turns; `r49d_recorded_*` request fixtures | Partial | Remaining cross-client and live acceptance verification |
| i | images | Generative Language parts | Codec and Code Assist rewrite carry inline and URL images in user and tool-result parts; `r49d_*` codec and recorded-request fixtures | Partial | Live acceptance verification |
| j | reasoning / thinking | Thought signatures, skip validator (`codeassist.go:29-31`) | `code_assist.rs` preserves native text/call signatures, skips unsigned turns, drops reasoning replay, and has `r49d_*` fixtures | Partial | Cross-client signature transport and live verification |
| k | quota / usage | `retrieveUserQuota` per model windows | `quota.rs` buckets from same RPC; R-49 applies the experiment-driven quota-id mapping | Done | — |
| l | model list | `retrieveUserQuota` preview signal + `listExperiments` flags choose Gemini CLI 0.61's catalog (`google.go:854-999`, `1180-1236`); `fetchAvailableModels` is Antigravity's path | `models.rs` ports the flag ids, catalog filtering, display names, and image flags; hourly per-instance cache; offline reference cases | Done | — |
| m | multi-account | Multiple google logins | multiple accounts | Done | — |
| n | error classification | RetryInfo → retry-after (`codeassist.go`); per-account tier refusal on `fetchAvailableModels` 400 (`google.go:330+`) | `tier_probe.rs` skips refused tiers for 24h, always ends with no tier; `wire` retry_delay in feature-parity doc; `r49_*` paused-clock fixtures | Done | — |
| o | daily check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | read gemini home only in reference docs | `AgentSignIn` declares `.gemini`; `discover.rs` lists/imports the active Google account and project read-only, rejects another OAuth client, and never persists the access token; `r49d_*` fixtures | Done | — |
| q | extras | Code Assist Standard/Enterprise project onboarding; `SetGoogleProject` names or clears an account's stored Cloud project; model-name mapping from the reference catalog | `project.rs` loadCodeAssist/onboardUser and leased account updates; `hofund accounts project gemini <email> <gcp-project-id>` trims, stores, and clears the project used by execute and quota; `models.rs` maps listed, outgoing, and quota ids from the same experiments; offline tests only | Done | — |

**Gaps**

- Live-account verification remains.
- All R-49d import, discovery, tool, image, and signature evidence is deterministic and offline.

---

## Google Antigravity

**Reference files read:** `google.go:82-100` (antigravity app, daily vs prod base), `antigravity_models.go`, `antigravity_pool.go`, `google_import.go`, `codeassist.go` (antigravity cloak, effort variants).

**Community:** `packages/antigravity-cloak/` (middleware for turned-away prompts).

**hofund path:** `plugins/subscriptions/antigravity/src/`; the existing desktop adapter remains separate under `plugins/agents/antigravity/`.

| # | capability | reference v0.1.3005 | hofund R-49d | status | gap to close |
|---|------------|----------------------|--------------|--------|--------------|
| a1 | sign-in: loopback OAuth (Antigravity client) | `google.go:82-90` embeds Antigravity's installed-app client, `/oauth-callback`, `localhost` | `sign_in.rs` and `lib.rs`: same scopes, PKCE, localhost path; client from plugin `client_id`/`client_secret` or `HOFUND_ANTIGRAVITY_CLIENT_*` env (hofund does not ship the 3005 literals). Offline tests use a fake client (`r49c_oauth_*`) | Done | Operator must supply the installed-app client; 3005 embeds it |
| b | import sources | `google_import.go` refresh token imports | `import.rs` accepts Cockpit/Manager/CLIProxyAPI exports and bare tokens, uses the configured Antigravity OAuth client, keeps file email/project fallbacks, and skips an existing refresh token; `r49d_*` recorded imports | Done | — |
| c | token storage | logins.json + in-memory refresh | Account store keeps the refresh token; `tokens.rs` keeps access tokens in memory | Done | — |
| d | refresh | same as Google subscriptions | `tokens.rs`: oauth2.googleapis.com, two-minute margin, lapsed grants; offline refresh/refusal tests | Done | — |
| e | upstream wire | `daily-cloudcode-pa.googleapis.com` + prod load base (`google.go:98-100`) | `Endpoints`, `project.rs`, and `upstream.rs` use those bases and the Antigravity Code Assist envelope | Done | — |
| f | inbound shapes | Same Code Assist envelope; effort variant IDs (`antigravity_models.go`) | SDK codecs translate inbound wires; `models.rs` folds families and `upstream.rs` picks effort variants; offline unary test | Done | — |
| g | streaming | SSE envelope | `upstream.rs` lifts each `response` frame on `streamGenerateContent?alt=sse`; offline stream test | Done | — |
| h | tools | tool declarations and thought signatures (`internal/gateway/codeassist.go`) | `plain_code_assist_tools` plus `code_assist.rs` sanitize ids, pair/dedup calls, select `VALIDATED` for Claude, and apply signature rules; `r49d_recorded_*` fixtures | Partial | Remaining cross-client and live acceptance verification |
| i | images | inline image parts on the Code Assist wire (`internal/gateway/codeassist.go`) | Codec and Code Assist rewrite carry inline and URL tool-result images; Claude responses lead, followed by labelled images; `r49d_*` fixtures | Partial | Live acceptance verification |
| j | reasoning | thought signatures / thinking config, cloak and turned-away detection (`codeassist.go`, `packages/antigravity-cloak`) | Effort/cloak/refusal tests plus `code_assist.rs` skip-signature handling and `r49d_*` recorded fixtures | Partial | Cross-client signature transport and live verification remain |
| k | quota / usage | `fetchAvailableModels` model quota plus `retrieveUserQuotaSummary` shared windows | `quota.rs` reads both, including reset times and omitted-fraction exhaustion; offline test | Done | — |
| l | model list | `antigravity_pool.go:15-49` union across accounts; effort folding (`antigravity_models.go`) | `models.rs` parses picker order, filters hidden/disabled ids, falls back through tiers, folds families, and merges account unions; offline tests | Done | — |
| m | multi-account | Pool catalog union | One Google email has distinct Gemini and Antigravity account ids; all stored Antigravity accounts join the union in account order | Done | — |
| n | error classification | 429 “turned away” prompts (`codeassist.go:51-66`) | `upstream.rs` distinguishes prompt refusal from real quota and returns a non-cooling 400 with the reference hint | Done | — |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | Antigravity desktop agent separate | Desktop `agent.toml` only; no subscription sign-in switch | Missing | No safe reference writer; switching remains out of scope |
| q | extras | Ban-risk warning in UI; version manifest URL (`google.go:68`) | Full account-suspension warning prints before CLI sign-in and remains in docs; request version is the fixed reference fallback | Partial | Dynamic version-manifest refresh remains |

**Gaps**

- Agent sign-in switching and dynamic app-version refresh remain.
- R-49d adds deterministic refresh-token imports and tool/image/signature fixtures; real-account verification is not performed.

---

## SuperGrok (`grok`)

**Reference files read:** `grok.go`, `grok_accounts.go`, `grok_install.go`, `grok_usage.go`, `migrate_side.go`.

**Community plugin:** `packages/grok/index.mjs` (CLI login, signing, refresh via `grok models`).

**hofund path:** `plugins/subscriptions/grok/`.

| # | capability | reference v0.1.3005 | hofund R-50 | status | gap to close |
|---|------------|----------------------|-------------|--------|--------------|
| a1 | sign-in: CLI device (`grok login`) | `grok.go:13-17`, plugin `index.mjs:1-9` | `lib.rs:3-7`, bridge flow runs `grok login --device-auth` | Done | — |
| a2 | browser / API key / paste | device via CLI only | N/A | N/A | — |
| b | import sources | Read CLI `auth.json`; reference-owned homes (`index.mjs:48-56`) | `discover.rs`, `creds.rs` import paths | Done | — |
| c | token storage | CLI home + plugin store | account secret + CLI home path in blob | Done | — |
| d | refresh | CLI `grok models` renew, one joined run per home, torn-read last-good fallback (`index.mjs`, `refresh.test.mjs`, `renew.test.mjs`) | `refresh.rs` holds the account lease across the CLI run, re-reads after waiting, joins completed renewal, and uses the explicit `creds::Cache`; request and hook leads stay at five minutes | Done | Deterministic lease/scripted-child coverage only |
| e | upstream wire | `https://cli-chat-proxy.grok.com/v1` OpenAI Responses + signed headers | `lib.rs:64-66`, `upstream.rs:73-80` | Done | — |
| f | inbound shapes | Responses upstream; body rewrite in plugin | `ProtocolId::OPENAI_RESPONSES`, `shape.rs` | Done | — |
| g | streaming | SSE | wire streaming | Done | — |
| h | tool calls | Allowed tool types set and opt-in `x_search` (`index.mjs:33-35`, `tool-choice.test.mjs`, `x-search.test.mjs`) | `shape.rs` keeps exactly the eleven types and handles automatic/null tool lists without injecting over explicit choices | Done | — |
| i | images | `input_image` in shape tests | `shape.rs:147-149` | Done | — |
| j | reasoning | reasoning items in Responses | `shape.rs:106+` | Partial | unverified vs xAI backend |
| k | quota / usage | plugin usage hooks; legacy `grok_usage.go` | `quota.rs` allowance and non-blocking on-demand windows; quota renews once on 401 | Partial | Fractional percentages, explicit span, and recent-429 display window are not represented by `QuotaWindow`; the reference shows no deferred/step-billing fields to add |
| l | model list | CLI/backend model API | `models.rs`, including one 401 renewal | Done | — |
| m | multi-account | reference-owned homes per extra account | multiple accounts | Done | — |
| n | error classification | missing sign-in is NeedsSignIn; an unrenewed token stays a plain failure with home-specific guidance | `refresh.rs` maps a missing home to Auth and unrenewed/expired-after-renew to non-Auth 401 with the corresponding guidance | Done | — |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | read `GROK_HOME` | `lib.rs:46` AgentSignIn, discover/import | Done | — |
| q | extras | CLI discovery and install guidance (`grok_install.go`, `index.mjs:26-31`, `76-92`) | `cli.rs` checks `GROK_BIN_DIR`, `GROK_HOME/bin`, `~/.grok/bin`, PATH, and `~/.local/bin` in order, accepts PATH/local only for the CLI's own install, and prints the reference command and `GROK_BIN_DIR` guidance | Done | No installer added |

**R-50 completed**

- Added the reference binary lookup order and missing-CLI install guidance without inventing an installer.
- Serialized hook and request renewal through the account lease, with post-lease rechecks, last-good torn-file fallback, and one 401 renewal for requests, models, and quota.
- Completed the tool-type/search fixtures and the missing-sign-in versus unrenewed error wording.
- All added tests use deterministic fixtures, recording hosts, or scripted children; no live-account verification was performed.

**Remaining gaps**

- Reasoning remains unverified against the backend.
- `QuotaWindow` rounds fractional percentages and has no explicit span or recent-429 display window. The reference's allowance and on-demand values are otherwise retained; no deferred or step-billing fields appear in the frozen source, so none are invented.

---

## Qoder (international + Qoder CN)

**Reference files read:** `qoder.go`, `migrate_qoder.go`, `qoder_checkin.go`; plugin `packages/qoder/index.mjs` (COSY SSE, device PKCE, VPC enterprise, two provider ids `qoder` / `qoder-cn`).

**hofund path:** `plugins/subscriptions/qoder/` (single plugin id `qoder`, site in credential).

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: device / PKCE | plugin `index.mjs:6-10` desktop PKCE device flow | `sign_in.rs`: S256 challenge, nonce and machine id, device poll, job exchange, user info; CN device-token fallback | Done | — |
| a2 | browser OAuth | the sign-in page is the device flow in a1, not a second OAuth grant | same device page; no invented second grant | N/A | — |
| a3 | API key / paste | import credential JSON | `import.rs:7-17` user paste import | Partial | PAT exchange remains |
| a4 | agent credential discovery | no agent credential export; native runtime helper only | none; no invented credential path | N/A | — |
| b | import sources | legacy `qoder.json` migrate (`qoder.go:47-74`) | JSON import only | Partial | Legacy file migrate remains |
| c | token storage | plugin-owned; job + refresh tokens | `creds.rs` SecretObject, with separate device pair, CN device-chat marker, and VPC | Done | — |
| d | refresh | single-use job/device refresh tokens, renew serialization, and relogin gap | `refresh.rs`: account lease, immediate pair commit, device rotation; `upstream.rs`: forced renew and five-minute relogin gap | Done | — |
| e | upstream wire | SSE `agent_chat_generation` + COSY envelope (`index.mjs:45-54`) | `upstream.rs`, `cosy.rs`, `shape.rs`: exact path/query, shuffled body codec, RSA/AES/MD5 envelope, native identity and model headers | Done | — |
| f | inbound shapes | OpenAI chat in IR → Qoder codec in plugin | SDK codecs into `shape.rs`: system, messages, tools, images, reasoning, cache marks, context and efforts | Done | — |
| g | streaming | COSY SSE `agent_chat_generation` | `stream.rs`: incremental envelope/chunk decoding, reasoning, native arguments, trailing usage and terminal frames; `r54b_selected_display_name_replaces_upstream_auto_in_unary_and_streaming_replies` proves selected-name identity in unary and streaming replies | Done | — |
| h | tool calls | JSON and XML-in-text lifted in plugin | `stream.rs`: wrapped JSON/Qwen XML plus bare and namespaced Anthropic XML, with byte-for-byte passthrough unless a declared valid call is lifted; `shape.rs`: following explicit result ids reserved before unknown-id pairing; native indexes and nameless retry; deterministic `r54b_unknown_result_id_does_not_take_an_explicitly_named_call` and `r54b_xml_that_is_not_a_declared_call_passes_through_byte_for_byte` | Done | — |
| i | images | image blocks in requests, including tool results | `shape.rs` text/image blocks and tool-result image carry-forward; fixture coverage | Done | — |
| j | reasoning | model config, efforts, streaming and prior assistant reasoning | `models.rs`, `shape.rs`, `stream.rs`; default/nearest efforts and `reasoning_content` round trip | Done | — |
| k | quota / usage | account-page usage on the device token, with rotation and free-model exclusions | `quota.rs`: `/sash/api/v2/me/usage`, pool windows, device retry and free exclusions | Done | — |
| l | model list | `/algo/api/v2/model/list` with COSY device headers | `models.rs`: native signed list, app/chat scenes, windows, images, efforts and free flags; `upstream.rs`: selected listing display name replaces upstream `auto` in every answer and stream chunk | Done | — |
| m | multi-account | separate subscriptions `qoder` and `qoder-cn` (`qoder.go:28-31`) | one plugin, `Site` plus VPC in creds; VPC account ids are distinct | Partial | Unified plugin id documented; separate routing labels remain |
| n | error classification | lapsed / NeedsSignIn only on refused refresh; queue and request codes | refused refresh lapses; chat 401/403 renews once with relogin gap; quota/queue statuses | Partial | Full queue wait and remaining request-code nuances remain; no #612 claim |
| o | daily check-in | `qoder_checkin.go` + plugin campaigns with native identity and device rotation | `checkin.rs`: device token, native identity, VPC rebasing, one device rotation | Partial | 30-minute recheck scheduling remains |
| p | agent sign-in / device identity | device PKCE sign-in and native runtime identity | native PKCE plus cached global/CN runtime identity and sanitized hostname | Done | — |
| q | extras | Qoder CN VPC instance hosts (`index.mjs:29-36`); COSY_VERSION desktop | `site.rs`: strict instance parsing and fail-closed web/openapi/gateway hosts on sign-in, refresh, models, chat, usage and check-in | Done | — |

**Gaps**

- R-54b landed native COSY SSE, request shaping and tool/image/reasoning handling, desktop PKCE, device/job refresh and rotation, native models and usage, and fail-closed CN VPC hosts. No agent credential discovery is planned from the reference's native runtime helper.
- PAT exchange and legacy `qoder.json` migration remain.
- The site-qualified single plugin remains: separate `qoder` / `qoder-cn` routing labels remain, while account ids distinguish public and VPC deployments.
- Queue waiting and the remaining request-code nuances, 30-minute check-in recheck scheduling, and live-account verification remain. No #612 claim is made.

---

## CodeBuddy / WorkBuddy (CN + international)

**Reference files read:** `workbuddy.go`, `workbuddy_checkin.go`, `migrate_workbuddy.go`.

**Community plugin:** `packages/workbuddy/index.mjs` (browser sign-in, desktop auth files, `/v2` chat, restriction code 11140).

**hofund path:** `plugins/subscriptions/workbuddy/`.

| # | capability | reference v0.1.3005 | hofund R-55c | status | gap to close |
|---|------------|----------------------|--------------|--------|--------------|
| a1 | sign-in: browser (Tencent page poll) | plugin `index.mjs:5-7`, state/token/account polling | `sign_in.rs` starts the returned HTTPS page, then polls token and account state for either `--site` (`r55_browser_sign_in_works_for_both_sites`, mocks only) | Done | — |
| a2 | sign-in: API key | API-key accounts (`packages/workbuddy/index.mjs:404-418` `keyAccount`, `apikey.test.mjs`) | `sign_in.rs::import_api_key` stores a key account with the anonymous suffix; chat sends bearer plus `X-API-Key`, never `X-Domain`; key quota and check-in make no calls (`r55_api_key_sign_in_works_for_both_sites_without_exposing_key`) | Done | — |
| a3 | desktop login reuse | read `workbuddy-desktop.info` / AI variant | `discover.rs:14-31`, `auth_store.rs` | Done | — |
| b | import sources | desktop JSON under CodeBuddyExtension data | same paths in discover | Done | — |
| c | token storage | plain JSON on disk (plugin) | encrypted hofund secret + desktop path in blob | Done | — |
| d | refresh | desktop token re-read + OAuth refresh in plugin | `refresh.rs` ensure_fresh / desktop re-read; encrypted access or refresh objects fail closed with a secret-free browser-sign-in error (`r55_desktop_rejects_encrypted_tokens_without_exposing_envelopes`) | Done | No decryptor exists in the frozen reference; none is invented |
| e | upstream wire | `{endpoint}/v2` streamed chat, gzipped body with `Content-Encoding: gzip` (`index.mjs:7-9`, `1502-1519`) | `upstream.rs` targets `{endpoint}/v2/chat/completions` and enables the SDK's opt-in request gzip, using `inflate::compress`; `a2c_24_upstream_sends_workbuddy_user_agent` inflates the recorded body and checks the exact URL and encoding | Done | — |
| f | inbound shapes | OpenAI chat + WorkBuddy headers | OpenAI chat + `creds::signed_headers` and `chat_headers` | Done | — |
| g | streaming | stream only in plugin | wire streaming | Done | — |
| h | tools | `fitTools`, `tools.test.mjs`, `aftertools.test.mjs` | `tools.rs` fits schemas and names, restores streamed and unary caller names, and inserts the empty assistant reply after tool results; `upstream.rs` learns the 11153 retry per site/model (`r55c_tool_*`, `r55c_after_tools_*`, recorded HTTP) | Done | — |
| i | images | `/v2/images/*`, `images.test.mjs`, `declared-images.test.mjs`; `draw_workbuddy.go` sends edit images as data-URL arrays | `models.rs` handles live and declared image flags, international GLM text-only models, drawing tags, and edit companions; `upstream.rs` forwards generation/edit JSON without gzip, normalizes one data-URL string to a one-element edit array, and unwraps the envelope; retrieval serves `/v1/images/generations` and `/v1/images/edits` (`r55c_image_*`, `r55c_model_images_*`, both sites, recorded HTTP) | Done | — |
| j | reasoning | effort lists, `reasoning.test.mjs`, `efforts.test.mjs`, `default-effort.test.mjs` | `tools.rs` replays `reasoning_content` and `reasoning_details` as `reasoning`; `models.rs` preserves effort lists and validates `defaultEffort`, else `effort`, against each model's levels; `upstream.rs` applies a configured default when no effort is named, preserves explicit efforts including `none`, and suppresses only the default for positive token budgets up to 1024 (`r55c_reasoning_*`, `r55c_default_effort_*`, `r55c_explicit_effort_*`, `r55c_small_budgets_*`, `r55c_effort_lists_match_the_frozen_config`) | Done | — |
| k | quota / usage | `usageOf`, `kindsOf`, `listedExtra`, `packagesOf`, and `enterpriseUsageOf` read the summary, paid/free package, and enterprise meters for the profile usage page | `quota.rs` splits plan and extra windows from groups or reference codes, retains truthful prepaid remainders without totals, adds unlisted extras, applies UTC+8 plan/enterprise resets, and tolerates detail-meter refusals; key accounts return no windows without calling billing (`r55b_*`, recorded HTTP only) | Done | Fields that `QuotaWindow` cannot represent are documented below |
| l | model list | `/v3/config` in plugin | `models.rs` reads live config with signed client headers and keeps declared per-site catalogs as fallback, with context/output, image, drawing, and effort metadata | Done | — |
| m | multi-account | `workbuddy` vs `workbuddy-ai` provider ids | one plugin keeps `site::Site` in every credential and derives account ids from `workbuddy` or `workbuddy-ai` plus uid; the labels select distinct endpoints, auth files, headers, and agent sign-ins | Partial | Not split: the frozen reference requires the two routing labels, which the site-qualified accounts retain, but shows no behavior that requires two hofund subscription ids |
| n | error classification | `WBRestricted` code 11140 (`workbuddy.go:267-299`) | `errors.rs` accepts top-level, `data`, or `error` numeric/string codes and appends the exact hint; the router applies a generic 403 account rest (`r55_restriction_*`, upstream test) | Done | — |
| o | daily check-in | `workbuddy_checkin.go` + plugin; CN only | `checkin.rs`, `with_daily_checkin`; international and key accounts are inactive | Done | — |
| p | agent sign-in | desktop auth paths | `AgentSignIn` workbuddy + workbuddy-ai (`lib.rs:31-35`) | Done | — |
| q | extras | User-Agent must be WorkBuddy/version (`index.mjs:8-9`) | `creds.rs` sends the reference's 5.9.3 app/2.165.0 CLI identity, attribution and chat SDK headers per site (`r55_signed_headers_match_each_site`, `r55_chat_headers_match_the_desktop_cli`) | Done | — |

`QuotaWindow` carries rounded integer percentages, optional reset times, and blocking state. The reference's credit amounts and display strings, plan labels, units, cycle spans, and `runsOutAt` / `runsOut` package-expiry fields have no faithful representation and are omitted. A prepaid remainder appears in a balance-style window name without a guessed total.

**R-55 completed**

- Browser and API-key sign-in work for both sites through the existing subscription proof flow, with deterministic recorded-HTTP tests only.
- Chat completions use `/v2` with gzip request bodies; billing and other non-completion calls stay uncompressed.
- Profile usage reads plan/extra/prepaid summary windows, paid/free package details, plan cycle resets, and the enterprise fallback.
- Chat, models, quota, check-in, and refresh use the reference's client identity and applicable signing headers; key material is sensitive and never included in errors.
- Code 11140 produces the reference hint; the router applies a generic 403 account rest.
- Encrypted desktop tokens remain unreadable by design and direct the user to browser sign-in without rendering the encrypted envelope.

**R-55c completed**

- Tool schemas and names are fitted and restored, with the learned 11153 empty-assistant retry for tool-result turns.
- Image generation and edits use the reference's `/v2/images/*` routes as plain JSON; a single data-URL edit string becomes a one-element array, and live and declared catalogs carry image and edit capabilities.
- Assistant reasoning history uses the reference field, live and declared model lists expose the reference effort levels, and chats without an explicit effort use a valid configured default unless a positive token budget is 1024 or less.
- All new behavior is covered by deterministic recorded HTTP or fixtures; no live-account tests were added.

**Remaining gaps**

- Separate subscription ids remain Partial as listed in row m; the two reference routing labels are preserved without a split.

---

## Z Code (`zcode`)

**Reference files read:** `zcode.go`, `zcode_device.go`, `zcode_team.go`, `migrate_zcode.go`, `plugin_checkin.go`, `workbuddy.go`, `internal/agent/zcode.go`, and `internal/appdir/zcode.go`. The earlier audit incorrectly said the plugin package was absent. The frozen community package is present at `packages/zcode` under community commit 1ad712d and was read for R-57b: `index.mjs`, `prompt.mjs`, the sign-in, Start Plan, model, gift, team, usage, and check-in tests, and the recorded marketing fixture. R-57c also read `dual.test.mjs`, `toolsearch.test.mjs`, and `concurrency.test.mjs`. Wire details below now come from that package as well as the Go sources.

**hofund path:** `plugins/subscriptions/zcode/`; R-57, R-57b, and R-57c use deterministic recorded HTTP and fixture tests only.

| # | capability | reference v0.1.3005 | hofund 08035c9 + R-57 + R-57b + R-57c | status | gap to close |
|---|------------|----------------------|--------------------------------|--------|--------------|
| a1 | sign-in: device / portal OAuth | `authorize()` posts `/api/v1/oauth/cli/init`, validates an HTTPS page, sets the site-specific redirect, and polls `/api/v1/oauth/cli/poll/{flow_id}` | `sign_in.rs` pending sessions implement both sites, business-token exchange, own/team plan resolution, and Start Plan fallback only for a live or pending plan | Done | Recorded mocks only; no live account |
| a2 | API key import | Plugin supports Z.ai and BigModel key plans | Agent-store import plus `SignInProof::Token` API-key accounts | Done | — |
| a3 | paste / browser | Automatic browser completion; the reference names no additional paste URL | `start_sign_in` returns an HTTPS `OpenUrl`; `complete_sign_in(Await)` polls and stores the account | Done | No paste flow is invented |
| b | import sources | encrypted `~/.zcode/v2/credentials.json` (`zcode.go:14-18`) | `creds.rs` decrypt + `discover.rs`, including JWT-only Start Plan stores | Done | — |
| c | token storage | logins.json for reference-added; encrypted agent store read | SecretObject + per-account device id (`creds.rs`) | Done | — |
| d | refresh | The frozen plugin says the JWT cannot be refreshed; app-backed accounts reread the app's renewed session | `refresh.rs` re-imports through the account lease and fails closed on an expired browser-only JWT; no renewal endpoint is invented | Partial | Recorded mocks only; browser-only JWTs require sign-in again |
| e | upstream wire | Plan bases and `START_BASE`; 3.17.2 headers and prompt texts, body dressing, device/session metadata, gift aliases, default-on free-first, ToolSearch aliases, busy retries, and edge-firewall holds | `upstream.rs`, `routing.rs`, `tool_search.rs`, `start.rs`, and `gift.rs` implement JWT-only, trial, and dual-account free-first routing with plan replay, exact prompt hashes, reasoning, live-bucket checks, ToolSearch request/reply renaming, six fixed busy waits, and a shared three-minute edge hold. Team keys skip personal-plan detection, and expired JWTs leave key-backed plan requests usable | Done | Deterministic recorded HTTP and fixtures only; no live account |
| f | inbound shapes | Messages only upstream | `upstream.rs` rejects non-Messages | Done | — |
| g | streaming | Anthropic SSE | wire streaming | Done | — |
| h | tool calls | Anthropic tools, including ToolSearch renamed to `ToolSearchTool` and restored on replies | Standard Messages wire plus request undefer/placeholder removal and unary/streamed call-name restoration in `tool_search.rs` | Partial | Deterministic fixtures cover ToolSearch; other tool behavior remains unverified |
| i | images | Per-model `supportsImage` in provider rules (`index.mjs:1690`) | `provider_config.rs` maps the flag to `ModelSpec::image_input` and input modalities | Done | Recorded config fixture only |
| j | reasoning | Per-model levels and thinking budgets (`index.mjs:1691`, `1753-1765`) | Provider levels map to `ModelSpec::efforts`; `start.rs` maps `none` to disabled thinking and other levels to the reference budgets | Done | Recorded config and wire fixtures only |
| k | quota / usage | Monitor quota plus Start Plan balance buckets; a live sibling supersedes a spent bucket for the same model | `quota.rs` monitor endpoints and `gift.rs` live/pending windows; spent siblings are non-blocking and recorded quota and trial-routing fixtures cover live-plan selection | Done | — |
| l | model list | Latest remote/local provider config, static fallback, and gift-tied model entries | `provider_config.rs` chooses by revision; `models.rs` adds Start and `-Trial` entries from every live grant | Done | No 10-minute config cache; mocks only |
| m | multi-account | team seats from `setting.json`; separate `(team)` labels | `team.rs` project keys and separate labels; native sign-in supports team selection | Done | — |
| n | error classification | ZCodeRestricted hint code 3012 and two-hour rest | `http_auth.rs` keeps HTTP 405, adds the support hint and cooldown | Done | — |
| o | daily check-in | `claimDaily` reads marketing/touch and posts `/billing/claim`; codes 0/1003/1004, captcha fail-closed, 30-minute recheck | `checkin.rs` maps all outcomes without a captcha solver; `CheckinResult::recheck_at` and daemon-persisted deadlines rerun inactive, ineligible, and failed claims after 30 minutes | Done | Recorded mocks only |
| p | agent sign-in switch | Go writes gateway provider config; team keys are created when absent; the encrypted account store is read-only in the reference | `team.rs` creates `zcode-team-api-key` with `keyType: 2`; `agent_sign_in.rs` still refuses credential switching | Partial | No safe encrypted-store account-switch writer is specified, so switching stays fail-closed |
| q | extras | `X-Device-Mid`; plugin pins `APP_VERSION` 3.17.2 and the marketing/Start version headers | `lib.rs` pins 3.17.2; `api.rs`, `checkin.rs`, and `start.rs` send the reference UA and `X-ZCode-App-Version` where used | Done | — |

**R-57 completed**

- Kept `lapsed` across same-secret team refresh re-imports, serialized re-import through the account lease, failed closed on relative credential paths, and added separate team labels (#613 follow-ups).
- Mapped code 3012 to the reference's user-facing risk-control hint and two-hour cooldown.

**R-57b completed**

- Used the present frozen plugin package to add 3.17.2 identity and exact 3.17.2 Start Plan prompts, native Z.ai/BigModel sign-in, Start Plan and gift-balance routing, provider-config image/reasoning metadata, missing team-key creation, and daily marketing claims.
- Added a plugin API recheck deadline and daemon persistence so inactive, ineligible, and failed Z Code results rerun after 30 minutes without sleeps or wall-clock test dependencies.

**R-57c completed**

- Added default-on free-first routing for dual accounts, with `freeFirst: false` as the opt-out, per-model spent/refusal rests, and same-request coding-plan replay for gift failures and quota envelopes.
- Added ToolSearch request/history/tool-choice rewriting, undefer and placeholder removal, and unary/streamed response restoration through the existing SDK wire hooks.
- Added 3008/3009 and wording-based busy classification, six fixed host-runtime waits, and a shared three-minute edge-firewall hold with the network hint and no account cooldown. Paused clocks drive all wait and expiry tests.

**Remaining gaps after R-57c**

- Agent credential switching remains fail-closed; the reference does not specify a safe encrypted-store switch writer.
- General tool behavior and refresh remain Partial as listed above. All new behavior is verified with recorded HTTP, fixtures, and paused clocks only; no live-account claim is made.

---
