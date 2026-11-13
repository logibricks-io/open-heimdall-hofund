# Subscription parity checklist (R-46)

Per-vendor, code-level parity audit of hofund subscription plugins against the frozen reference baseline **v0.1.3005** (tag commit 6abacd46; Go core under `internal/provider`, `internal/gateway`, `internal/claudebridge`) and the community auth plugins it ships with (the reference's community plugins repository at 1ad712d, the last commit before the v0.1.3005 tag). hofund side: main at **08035c9**.

Scope: the 14 subscriptions in the v0.2.0 plan (Claude Pro/Max, Cursor, Devin, Zed, ChatGPT/Codex plus ChatGPT API, Copilot plus GHE, Gemini Code Assist plus Antigravity, SuperGrok, Kiro, Factory, Cline, Qoder plus CN, CodeBuddy/WorkBuddy plus international, Z Code). Each section covers rows a-q: sign-in methods, import, token storage, refresh, upstream wire, inbound API shapes, streaming, tools, images, reasoning, quota, model list, multi-account, error classification, warm-up/check-in, agent sign-in switching, vendor extras.

This is a code-level checklist. Real-account smoke tests are out of scope and happen after the release candidate. Rows marked "unverified" are places where the audit could not confirm behaviour from code alone; the implementing task must confirm them.

Legend: **Done** = matches the reference at checklist granularity; **Partial** = subset, or mocks only; **Missing** = not implemented; **N/A** = the reference does not offer it.

## Summary

| Vendor section | Done | Partial | Missing | N/A | Unverified |
|---|---|---|---|---|---|
| Claude Pro/Max (`claude`) | 1 | 17 | 3 | 2 | 0 |
| Cursor (`cursor`) | 1 | 15 | 3 | 2 | 0 |
| Devin (`devin`) | 11 | 6 | 1 | 1 | 0 |
| Zed (`zed`) | 0 | 6 | 1 | 3 | 0 |
| Kiro (`kiro`) | 0 | 16 | 1 | 2 | 0 |
| Factory / Droid (`factory`) | 1 | 15 | 0 | 3 | 0 |
| Cline / ClinePass (`cline`) | 2 | 12 | 1 | 3 | 1 |
| ChatGPT / Codex (`codex` backend) and ChatGPT API (`chatgpt-api`) | 8 | 11 | 2 | 0 | 0 |
| GitHub Copilot (github.com and GHE) | 15 | 2 | 0 | 5 | 0 |
| Gemini CLI (Code Assist) | 8 | 6 | 2 | 2 | 0 |
| Google Antigravity | 10 | 4 | 2 | 1 | 0 |
| SuperGrok (`grok`) | 10 | 6 | 0 | 2 | 0 |
| Qoder (international + Qoder CN) | 1 | 10 | 9 | 0 | 0 |
| CodeBuddy / WorkBuddy (CN + international) | 13 | 3 | 0 | 0 | 3 |
| Z Code (`zcode`) | 7 | 8 | 2 | 0 | 2 |

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
| a1 | sign-in: browser / loopback via Claude Code | `startClaudeSignIn` runs `claude auth login --claudeai` in a temp dir, relays URL via `TookOpenedURL` / `openedURLEnv` (`claude_signin.go:147-198`) | `lib.rs` + `cli.rs`: same command in private dir, link relay + optional browser wait (`lib.rs:3-8`, `36-45`) | Partial | WSL fallback when Windows has no local `claude` (`claude_signin.go:85-111`) not ported |
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
| q | vendor extras | WSL Claude, per-login proxy (`claude_signin.go:139-145`), setup-token inference-only | Proxy/WSL/setup-token absent | Partial | Proxy env for sign-in and runs; setup-token (`R-5`, `R-6`) |

### Gaps (Claude)

- MCP subscription bridge and multi-protocol gateway translation — implemented on the R-6 stack (PRs #753–#761 and R-6g), not merged; real-account smoke remains **#304**
- Quota from `/usage` + stream header limits — **L**, **R-5**
- Model list and scoped quota windows — **M**, **R-5**
- macOS Keychain credential path — **M**, **R-5**
- Setup-token sign-in and accounts — **S**, **R-5**
- WSL fallback + per-login proxy wiring for Claude Code — **M**, **R-6** (new to the plan in this audit)

---

## Cursor (`cursor`)

**Reference files read:** `internal/provider/cursor.go`, `cursor_models.go`, `migrate_side.go`; `packages/cursor/index.mjs`, `native-session.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/cursor/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: browser PKCE | Plugin: `loginDeepControl` + poll (`cursor/index.mjs:6-8`, `386+`) | `sign_in.rs` + `SignInFlowKind::Pkce` (`lib.rs:32-35`, `170+`) | Partial | Parity with plugin poll/error mapping (unverified edge cases) |
| a2 | sign-in: cursor-agent reuse | Read keychain / `auth.json`, never write (`cursor/index.mjs:8-11`, `112-118`) | `cli.rs` keychain + `auth.json`; import/discover (`discover.rs`, `import.rs`) | Partial | Windows auth path parity (`authFile()` in plugin) |
| a3 | sign-in: API key → token | `CURSOR_API_KEY` → `/auth/exchange_user_api_key` (`cursor/index.mjs:347-360`) | No `CURSOR_API_KEY` handling in crate | Missing | API key exchange (`R-8`) |
| a4 | sign-in: device code | N/A | N/A | N/A | — |
| a5 | import paste | Core mover reads cli-config (`cursor.go:65-80`) | `import_accounts` JSON blob | Partial | — |
| b | import sources | Agent home `.cursor` auth + keychain | `AgentSignIn` + discover (`lib.rs:37-41`) | Partial | Team id from cli-config for billing (`cursor/index.mjs:120-177`) |
| c | token storage | Plugin session store; core mover side logins | Daemon secret blob + optional agent read-only | Partial | Persist team/active org metadata if needed for quota |
| d | refresh | `cursor-agent status` / token TTL (`cursor/index.mjs:182-194`, CLI_MARK) | `refresh.rs` + `session.rs` spawn status | Partial | Serialize refresh like plugin; API key token refresh |
| e | upstream wire | Connect bidi `AgentService/Run` + HTTP/1 RunSSE + BidiAppend (`cursor/index.mjs:13-17`, `44-46`) | `run.rs` incrementally reads either transport, answers KV/exec and selected field-36 tool lists, sends heartbeats, and enforces monotonic idle/quiet/tool-count limits; `upstream.rs` opens one stateless Run per execute, retries one regional failure after fresh host discovery, and retries once on a `maxRequired` refusal when the first attempt was not Max Mode | Partial | Images and effort (**R-9**) |
| f | inbound shapes | OpenAI-compatible chat to plugin fetch (`cursor/index.mjs:29-30`) | Canonical IR text and tool history maps to stable JSON transcript blobs plus the protobuf user turn in `request.rs`; selected tools become context/wrapper definitions and the dynamic catalog; gateway codecs supply the IR | Partial | Images (**R-9**) |
| g | streaming | Connect stream / RunSSE (`cursor/index.mjs:13-17`) | `Completion::Stream` starts after the first text delta or caller tool event, then emits text or full-argument tool blocks and the clean EndTurn/ToolUse ending; an empty turn is a non-cooling 502, pre-output stalls have idle/quiet diagnostics, and post-output stalls say the upstream stopped sending | Partial | Usage and reasoning (**R-9**) |
| h | tool calls | MCP `CallDynamicTool` (`cursor/index.mjs:21-26`, `51`) | Selected MCP execs emit caller tool blocks; unknown/Cursor-owned execs stay refused; server-completed calls are not emitted; counts, checkpoints, and the 60-second count wait seal steps | Partial | Streaming argument deltas and Cursor-owned tools are deferred |
| i | images | unverified in core; plugin native session handles blobs | Not served | Missing | **R-9** |
| j | reasoning | Plugin tests `reasoning.test.mjs` | Not served in this coverage snapshot; R-9b later landed thinking blocks | Missing | **R-9** |
| k | quota / usage | Connect `GetCurrentPeriodUsage`, team header (`cursor/index.mjs:171-176`, `connect.rs` parity) | `quota()` via Connect unary (`lib.rs:306-322`, `connect.rs:9-38`) | Partial | Deferred/step usage, credit events (`cursor/deferred-usage.test.mjs`) — **R-9** |
| l | model list | Plugin catalog + agent list; core `cursor_models.go` contexts | `cursor-agent models` parse + family fold (`models.rs`, `lib.rs:337-362`) | Partial | Server-side catalog merge like plugin (`catalog.test.mjs`) |
| m | multi-account | Team id per token (`activeTeamId`) | Multiple accounts; team header not evident in hofund Connect calls | Partial | `x-cursor-team-id` on chat + usage (**R-8**) |
| n | error classification | Plugin maps budget/refusal (`errors.test.mjs`) | `failure.rs` implements the text-only section 6 table: regional/permission 403, status-style 401, quota vs rate 429, context overflow and invalid input 400, unavailable 503, other Connect codes 502, and unchanged bare HTTP statuses. R-9d retries once on a `maxRequired` refusal when the first attempt was not Max Mode | Partial | — |
| o | warmup / keepalive | none found (no keepalive in the Cursor core or its auth plugin) | None | N/A | — |
| p | agent sign-in | Core `cursorStatus` mark (`cursor.go:59-80`) | discover/import (`lib.rs:383-396`) | Done | — |
| q | extras | HTTP/2 fallback, ghost mode, client version string (`cursor/index.mjs:48`, `connect.rs:24-26`) | Run headers and HTTP/2 transport plus RunSSE/BidiAppend fallback in `run.rs`, with loopback tests | Partial | Team-id discovery hook is still optional and production execute passes `None` |

### Gaps (Cursor)

- Images and effort remain later R-9; Max Mode retry landed in R-9d — **L**, **R-9**
- CURSOR_API_KEY exchange sign-in — **M**, **R-8**
- Team billing header — **S**, **R-8**

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
| d | refresh | Plugin renews tokens (unverified); core reads CLI | `refresh` loads stored only (`lib.rs:174-176`) | Partial | OAuth refresh if CLI tokens expire (**R-12**) |
| e | upstream wire | Protobuf Connect `GetChatMessage` on `server.codeium.com` (`devin/index.mjs:28-32`) | `upstream.rs` unary POST with literal Basic auth, 300 s read budget, origin override, and tool field 10 | Done | — |
| f | inbound shapes | Chat completions + Anthropic for Claude models (`devin/index.mjs:19-26`) | Chat, Responses, and Messages IR execute; catalog advertises Chat endpoint only | Partial | Per-model inbound routing (**R-13**) |
| g | streaming | Connect stream from GetChatMessage | Incremental text, reasoning, tool-start, and argument-delta IR events; content-or-tool-gated success | Done | — |
| h | tool calls | Plugin `tool-ids.test.mjs`, split calls | R-13b encodes field-6 calls and field-7 results, joins split calls, fills interrupted results, sanitizes ids, offers history-only tools, and streams tool-only replies with usage | Done | — |
| i | images | Plugin `images.test.mjs`, `devin_images.go` in core catalog | R-13b encodes inline field-10 images, omits URL-only images, and keeps direct or `FROM`/`NOTE`-associated tool-result images on their tool prompts | Done | — |
| j | reasoning / thinking | `thinking-signature.test.mjs`, effort variants | R-13b replays unsigned reasoning on non-Claude uids, signed Claude thinking on fields 11/12/18, and fragmented Claude signatures from fields 10/21 with a 512-entry type map | Done | — |
| k | quota / usage | Plugin `usage.test.mjs` | R-13a `quota.rs` reads daily/weekly and informational ACU windows; free ids survive as model exclusions; field 7 becomes finish usage with cache counters and output estimation | Partial | Plan/balance presentation and organization metadata are not represented by `Subscription::quota` |
| l | model list | CLI JSON + `GetCliModelConfigs` (`devin/index.mjs:44-134`) | Live raw-protobuf RPC, isolated CLI fallback, embedded snapshot, per-account freshness/retry cache, tier-4 free flags, and matching-variant list prices | Done | — |
| m | multi-account | EU host `eu.codeium.com` (plugin) | unverified in hofund | Partial | Region selection (**R-12**) |
| n | error classification | Plugin `errors.test.mjs` | `errors.rs` section 6 HTTP/Connect mapping, exact empty-reply and broken-stream errors | Done | Tool/image-specific cases (**R-13**) |
| o | keepalive | `keepalive.test.mjs` | None | Missing | **R-13** (new to the plan in this audit) |
| p | agent sign-in | Core installs CLI (`devin.go:8-12`) | `agent_sign_in.rs` write TOML | Partial | CLI install helper like core (`DevinExecutable`) |
| q | extras | gzip request, identity headers, CLI version pin (`devin/index.mjs:33-36`) | `connect/` message tables + SDK `protobuf`; 8083/8333 gzip tests; HTTP encoding header, metadata, and sensitive Basic auth | Done | — |

### Gaps (Devin)

- Messages cache marks — **L**, **R-13**
- CLI install/bootstrap — **S**, **R-12** (new to the plan in this audit)
- Keepalive (`keepalive.test.mjs`) — **S**, **R-13** (new to the plan in this audit)

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

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: remote portal (Google/GitHub/Builder ID/IDC) | Plugin loopback ports + AWS OIDC (`kiro/index.mjs:3-8`, `37-38`) | Bridge defers to IDE/CLI; no portal in crate | Missing | Remote sign-in (**R-51**) |
| a2 | sign-in: reuse kiro-cli / IDE | `readCLI` / `readIDE` (`kiro/index.mjs:117-181`); core `kiro.go:19-22` | `discover.rs` reads `kiro-auth-token.json` under `KIRO_IDE_CACHE` | Partial | kiro-cli SQLite import (`readCLI`, `kiro.go:65-79`) |
| a3 | sign-in: API key `ksk_` | Plugin + core treat as API key | unverified — method `apikey` in upstream headers (`upstream.rs:54-55`) | Partial | Explicit API key sign-in UX |
| b | import | IDE cache file; CLI DB | Token file discover | Partial | CLI DB path (**R-51**) |
| c | token storage | Write-back to CLI DB or IDE file on refresh (`kiro/index.mjs:183-200`) | `creds.rs` write-back helpers for IDE path | Partial | SQLite write-back (**R-51**) |
| d | refresh | CLI first, else OIDC/refreshToken (`kiro.go:301-374`; plugin `refresh.test.mjs`) | `refresh.rs` lease + social/IDC; external-idp refused (`refresh.rs:88-91`) | Partial | kiro-cli delegated refresh; 400 lapsed mapping (**R-24**, **R-51**) |
| e | upstream wire | Event-stream `generateAssistantResponse` (`kiro/index.mjs:16-18`; `upstream.rs:44-48`) | Same endpoint and headers; `open_raw` feeds unary and streaming decoders | Partial | Mock/loopback verification only (**R-51**) |
| f | inbound shapes | Anthropic Messages only (`kiro/index.mjs:14-18`) | Messages only; Chat/Responses are not invented | N/A | — |
| g | streaming | AWS event stream to Messages SSE | `kiro_stream.rs` incrementally emits IR text, tool starts, argument deltas, block stops, and finish events | Partial | Mock/loopback verification only (**R-51**) |
| h | tool calls | Plugin tool tests (`tool-names.test.mjs`) | `kiro_body.rs` maps declarations, normalized ids/names, split calls, and matched/missing results; replies restore caller names | Partial | Mock/loopback verification only (**R-51**) |
| i | images | `image-size.test.mjs` | Inline user/tool-result images use format + `source.bytes`; only the latest image group is resent | Partial | Over-5-MiB Base64 images are refused; deterministic shrink/note behavior is not implemented (**R-51**) |
| j | reasoning / effort | `effort.test.mjs` | Enabled/adaptive thinking maps to the reference budget tags; structured output is added to instructions | Partial | Model-listed native effort fields and reasoning reply events (**R-51**) |
| k | quota / usage | `Get-Usage-Limits` (`kiro.go` comments; plugin `usage.test.mjs`) | `quota.rs` with profile ARN (`lib.rs:63-69`) | Partial | Builder ID profile ARN defaults (**R-51**) |
| l | model list | Plugin lists from API | `models.rs` | Partial | Profile-scoped list refresh |
| m | multi-account | uid by login provider (`kiro/index.mjs:53-65`) | Account ids from label/provider | Partial | Stable ids (#607) (**R-24**) |
| n | error classification | Plugin `errors.test.mjs`; refresh 400 → lapsed | Generic auth on 401/403 (`upstream.rs:60-63`) | Partial | **R-24** |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | IDE cache discover | `agent_sign_in.rs` + discover | Partial | CLI DB discover |
| q | extras | Builder ID profile ARN constant (`kiro/index.mjs:50-51`) | `BUILDER_ID_PROFILE` in `creds.rs:25-26` | Partial | External-idp refresh (**R-51**) |

### Gaps (Kiro)

- Live-account verification, oversized-image resizing, and native effort/reasoning fields — **M**, **R-51**
- kiro-cli SQLite import + refresh delegation + write-back — **L**, **R-51**
- Browser/portal sign-in — **L**, **R-51**
- external-idp refresh — **M**, **R-51**
- Refresh 400 lapsed + stable account ids — **S**, **R-24**

---

## Factory / Droid (`factory`)

**Reference files read:** `internal/provider/factory.go`, `factory_key.go`, `factory_usage.go`, `migrate_factory.go`; `packages/factory/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/factory/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: WorkOS device | Plugin WorkOS device + `CLIENT_ID` (`factory/index.mjs:12-16`) | `SignInFlowKind::Device` + `WORKOS_CLIENT_ID` (`lib.rs:28-35`) | Partial | Port the droid production WorkOS client from the plugin (`factory/index.mjs:15-16`); hofund `lib.rs:29` carries the Cline WorkOS client (`cline/index.mjs:16`) instead |
| a2 | sign-in: API key `fk-` | `factoryCreds.Key` (`factory.go:68-71`); plugin whoami | `import_api_key` (`sign_in.rs:138+`) | Done | — |
| a3 | browser OAuth loopback | N/A (device) | N/A | N/A | — |
| b | import | Mover reads built-in accounts | API key + device completes to store | Partial | — |
| c | token storage | JSON creds with org/region/prem (`factory.go:49-72`) | Secret blob via `tokens.rs` | Partial | `premBaseHost` routing |
| d | refresh | WorkOS refresh; API key no refresh (`factory/index.mjs:24-29`) | `tokens::refresh_account` | Partial | Lead times / grace (**R-52**) |
| e | upstream wire | Per-model `/api/llm/{a,o,g}/v1` (`factory/index.mjs:33-39`) | `upstream.rs` via `wire_target` (`factory/upstream.rs:41-57`) | Partial | `prem` host override (`factory.go:65-67`) |
| f | inbound shapes | Anthropic, Responses, Chat, Gemini (`factory/index.mjs:33-39`) | Same four wires (`models.rs:360-385`) | Partial | Fast mode / priority mapping (`fastBody`, `factory/index.mjs:128-158`) |
| g | streaming | Via AI SDK wire helpers | `wire::execute` (streaming depends on wire) | Partial | Verify Gemini stream path |
| h | tool calls | `anthropic-tools.mjs` | Through wire adapters | Partial | Anthropic tool unwrap tests |
| i | images | Model table `images` flag (`factory/index.mjs:59-116`) | Catalog flags | Partial | — |
| j | reasoning | Effort variants (`effort.test.mjs`) | Model efforts in catalog | Partial | Gemini thinkingConfig |
| k | quota / usage | GET `/api/billing/limits` (`factory_usage.go`; plugin `usage.test.mjs`) | `quota.rs` | Partial | Droid Core vs extra balance display |
| l | model list | Static MODELS + promotions (`factory/index.mjs:59-190`) | `models.rs` catalog | Partial | Promotion rateWas (**R-52**) |
| m | multi-account | Org active id header (`factory.go:57-60`) | `x-factory-org-id` (`upstream.rs:45-47`) | Partial | EU region base (`factory.go:75-79`) |
| n | error classification | `lapsed.test.mjs` | 401 → refresh retry (`upstream.rs:71-75`) | Partial | Content policy / rate rests |
| o | warmup | N/A | N/A | N/A | — |
| p | agent sign-in | N/A | N/A | N/A | — |
| q | extras | `X-Client-Version` pin (`factory/index.mjs:17-23`) | `client::chat_headers` | Partial | Version bump parity (**R-52**) |

### Gaps (Factory)

- Fast mode / service_tier / anthropic speed mapping — **M**, **R-52**
- Prem/custom host per org — **M**, **R-52**
- Promotions on model rates — **S**, **R-52**
- Use the droid production WorkOS client (hofund uses the Cline one) — **S**, **R-52**

---

## Cline / ClinePass (`cline`)

**Reference files read:** `internal/provider/cline.go`, `cline_usage.go`; `packages/cline/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/cline/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: WorkOS device | Plugin device + register (`cline/index.mjs:3-19`) | `DeviceFlow` + same `WORKOS_CLIENT_ID` (`cline/lib.rs:28-35`) | Partial | Cline register endpoint vs raw WorkOS (unverified) |
| a2 | sign-in: API key | Plugin key auth | `SignInProof` API key path in `sign_in.rs` | Done | — |
| a3 | browser loopback | N/A | N/A | N/A | — |
| b | import | Preset/key plan in core | OAuth/API key via sign-in only | Partial | — |
| c | token storage | Plugin session; core uses key | Secret blob + `workos:` prefix handling in tokens | Partial | Retryable grace window (`cline/index.mjs:27-30`) |
| d | refresh | WorkOS refresh + grace (`cline/index.mjs:25-30`) | `tokens::refresh_account` on 401 | Partial | Renew serialization (**R-24**, **R-53**) |
| e | upstream wire | OpenAI chat to `api.cline.bot` (`cline.go:43-49`; plugin) | `upstream.rs` OPENAI_CHAT + envelope unwrap | Partial | — |
| f | inbound shapes | Chat completions + desktop headers (`cline.go:15-31`) | Same headers `client.rs:4-18` (desktop 3.5.54) | Partial | Plugin uses **cline-cli** 3.0.68 identity (`cline/index.mjs:38-74`) — intentional mismatch to audit |
| g | streaming | Plugin stream chat | `wire::execute` | Partial | unverified parity |
| h | tool calls | Through chat API | wire layer | Partial | — |
| i | images | Catalog `input.image` per model and image-part handling (`packages/cline/index.mjs:356-357`, `482-518`) | no image handling found in `plugins/subscriptions/cline/src` | Missing | Image input per model catalog (**R-53**) |
| j | reasoning | unverified | unverified | unverified | Confirm in **R-53** |
| k | quota / usage | GET `/users/me/plan/usage-limits` + credits (`cline_usage.go:39-67`) | `quota.rs` | Partial | 404 without ClinePass handling |
| l | model list | `recommended-models` clinePass + free (`cline.go:56-61`) | `models.rs` filters clinePass/free | Partial | cloud-models feed if any (unverified) |
| m | multi-account | Multiple keys/accounts | Daemon multi-account | Done | — |
| n | error classification | NeedsSignIn on hard expire (`cline/index.mjs:27-30`) | 401 refresh path | Partial | **R-53** |
| o | check-in | N/A | N/A | N/A | — |
| p | agent sign-in discovery | N/A (agent uses provider JSON) | No discover (`subscriptions.md:100`) | N/A | — |
| q | extras | Task id header (`cline/index.mjs:62-75`) | Optional `X-Task-ID` in `client.rs:20-22` | Partial | Session→task id hook (**R-53**) |

### Gaps (Cline)

- Client identity parity (desktop vs CLI headers/version) — **M**, **R-53**
- Task/session headers on chat — **S**, **R-53**
- Retryable token grace + NeedsSignIn mapping — **S**, **R-24**, **R-53**
- OAuth register path parity with Cline app — **M**, **R-53**
- Image input per model catalog — **S**, **R-53**

---

## ChatGPT / Codex (`codex` backend) and ChatGPT API (`chatgpt-api`)

**Reference files read:** `internal/provider/signin.go`, `codex_store.go`, `import_codex.go`, `codex_request.go`, `codex_models.go`, `codex_limits.go`, `codex_credits.go`, `codex_resets.go`, `codex_zstd.go`, `codex_http1.go`, `codex_device_signin.go`, `codex_switch.go`, `warmup_daily.go`, `chatgpt_api.go`, `internal/gateway/codex_backend.go`.

**hofund path:** `plugins/subscriptions/codex/` (`lib.rs`, `sign_in.rs`, `device_sign_in.rs`, `tokens.rs`, `discover.rs`, `agent_sign_in.rs`, `upstream.rs`, `limits.rs`, `usage.rs`, `resets.rs`, `models.rs`, `warmup.rs`, `shape.rs`). **ChatGPT API:** no plugin; `docs/coverage/subscriptions.md` row `chatgpt-api` → missing.

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: browser / loopback OAuth (Codex CLI PKCE) | `signin.go:38-45` codex authorize URL, fixed `127.0.0.1:1455`, scopes; loopback callback | `codex/src/lib.rs:191-241`, `sign_in.rs:16-37` same client, port 1455, PKCE + loopback | Done | — |
| a2 | sign-in: device code | `codex_device_signin.go` (device auth APIs) | `device_sign_in.rs:27-30`, `lib.rs:196-206` `--device-auth` flow | Done | — |
| a3 | sign-in: API key (detect only) | Codex `auth.json` with `auth_mode: apikey` is not an account: discovery and agent switch drop it (`logins.go:767` `codexLoginIn`, `account.go:1044` `codexAccount`); `CodexAPIKeySignedIn` (`logins.go:571`) also lets `/responses/compact` pass a key's account holds (`gateway/codex_backend.go:693` `compactSigninHeld`) | `discover.rs:73-80` drops `auth_mode: apikey` on read/list/import; `agent_sign_in.rs:47` writes `auth_mode: chatgpt`; tests `r47a_a3_*` | Partial | Gateway `/responses/compact` compaction allowance for API-key-held requests has no hofund mirror on the subscription path (**R-47**, later) |
| a4 | sign-in: paste callback | `signin.go:69` PasteCallback on sign-in state | loopback failure falls back to pasted callback (`lib.rs:217-218` comment only) | Partial | Explicit paste-callback UX not verified in plugin API |
| a5 | sign-in: ChatGPT API (Sign in with ChatGPT / dynamic client) | `chatgpt_api.go:42-66` `dynamic_agent_client`, per-account client id, `siwcAuthorize` | no `chatgpt-api` plugin (`docs/coverage/subscriptions.md:25`) | Missing | Full SIWC plugin (R-47) |
| b | import sources | `codex_store.go:517+` read/write Codex `auth.json` and macOS Keychain; `import_codex.go` config.toml providers | `discover.rs:19-54` read `auth.json` only; `agent_sign_in.rs:9-71` write `auth.json` (file mode) | Partial | Keychain read/write, config.toml provider import (R-47) |
| c | token storage / encryption | `logins.json` + optional Codex home/keychain (`codex_store.go`) | daemon `secret_ref` encrypted store via `host.accounts()` (`crates/kernel/daemon/src/subscriptions.rs` pattern) | Partial | Bidirectional Codex store sync with Keychain (R-47) |
| d | refresh | OAuth refresh with rotation; Codex/keychain writers (`codex_store.go` write) | `tokens.rs:78-176` lease, `LAPSED` on refused refresh, 5 min margin | Partial | Persist refresh back to Codex Keychain when Codex rotates alone (R-47) |
| e | upstream wire | POST `https://chatgpt.com/backend-api/codex/responses`, zstd bodies (`codex_zstd.go:12-24`), HTTP/1.1 fallback (`codex_http1.go`) | `plugin-sdk` `codex_zstd.rs` + `wire.rs` zstd level 3 on POST `{base}/responses`; `upstream.rs` `chatgpt_codex_zstd(true)`; plugin HTTP uses the host unary pool built without reqwest `http2` (`crates/kernel/plugin-host/Cargo.toml:21`, test `pool.rs` `pooled_client_sends_an_http11_request_line`); a later Connect duplex client must pin unary dashboard calls to HTTP/1.1 (Cursor Connect design, row **e** above); tests `r47a_e_*`, SDK `r47a_e_*` | Done | — |
| f | inbound API shapes | Gateway `codex_backend.go` serves Codex path; pool accounts use Responses upstream | Plugin upstream `ProtocolId::OPENAI_RESPONSES` only (`upstream.rs:84`); gateway codec translates Chat/Responses/Messages/Gemini to IR | Partial | Codex-as-provider gateway mirror (`codex_backend.go`) not in hofund scope here; subscription path is Responses-only |
| g | streaming | SSE stream; fold for unary (`codex_request.go` patterns) | `upstream.rs:96-120` forces stream, fold when unary | Done | — |
| h | tool calls | Codex tool name mapping in gateway/provider | `upstream.rs:82-94` `codex_tool_names` restore | Done | — |
| i | images | via Responses content parts (gateway codex) | `tests.rs` `r47a_i_*`: Responses `input_image` in upstream POST body | Done | — |
| j | reasoning / thinking | Codex model instructions / reasoning items in gateway | `shape.rs` (referenced by upstream) drops unsupported fields | Partial | Parity with the reference reasoning compaction paths unverified |
| k | quota / usage | Headers + `codex.rate_limits` event; `/wham/usage`; credit resets (`codex_resets.go:180+`) | `limits.rs`, `usage.rs` headers, event, `/wham/usage`; `resets.rs` + `Subscription::quota_reset_credits` / `consume_quota_reset` on `/wham/rate-limit-reset-credits` (consume only on explicit calls); tests `r47a_k_*` | Partial | UI cards and daemon CLI for reset consume (R-47 later slice) |
| l | model list | `codex_models.go` backend list + instructions | `models.rs` `/codex/models`, cached prompts | Done | — |
| m | multi-account | Multiple logins; routing in provider | Multiple accounts via daemon subscription members | Done | — |
| n | error classification | Expired → refresh; spent windows; credits (`codex_credits.go`) | `tokens.rs` lapsed; `limits.rs` observe blocking vs credits | Partial | Remote reset / autoreset behaviours in the reference not ported |
| o | daily warmup | `warmup_daily.go` + provider warmup | `lib.rs:372-377` `daily_warmup` → `warmup.rs` | Done | — |
| p | agent sign-in switch | Writes Codex auth (`codex_switch.go`, store) | `write_agent_sign_in` → `agent_sign_in.rs` | Partial | Keychain mode + lock interaction with live Codex unverified |
| q | extras | `chatgpt-account-id`, Codex version headers, plan in ID token | `upstream.rs:172+` headers, `CODEX_VERSION`, `sign_in.rs:13` auth claim; zstd on Codex turns (row **e**) | Done | — |

**Gaps**

- Implement `chatgpt-api` subscription (dynamic OAuth, `api.openai.com/v1/responses`, agent instructions) — **L**, **R-47**.
- Codex Keychain + two-writer auth.json sync — **M**, **R-47**.
- Credit reset consume surfaced in CLI/control UI cards — **M**, **R-47** (plugin API and Codex `/wham/rate-limit-reset-credits` read/consume are in place; automatic consume is forbidden).
- Gateway `/responses/compact` compaction allowance when the held sign-in is API-key mode — **S**, **R-47** (subscription plugin already drops `auth_mode: apikey` accounts in discovery and agent switch).

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

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: loopback OAuth (Gemini CLI client) | `google.go:71-80` client id, `/oauth2callback` | `gemini/src/sign_in.rs`, `lib.rs:48-52` same client/secret | Done | — |
| a2 | device / API key / paste | N/A for Gemini CLI built-in | N/A | N/A | — |
| b | import sources | `google_import.go:178+` Cockpit/Manager/CLIProxyAPI exports; read `~/.gemini/oauth_creds.json` never write | no import in gemini plugin | Missing | Import paths + read-only gemini home (R-49) |
| c | token storage | `logins.json`; refresh in memory only | `tokens.rs:1-4` in-memory refresh, stored refresh unchanged | Done | — |
| d | refresh | `google.go` in-memory refresh at oauth2.googleapis.com | `tokens.rs:33-52` TokenCache + lapsed flag | Done | — |
| e | upstream wire | `cloudcode-pa.googleapis.com` `v1internal:{generateContent,streamGenerateContent?alt=sse}` envelope | `upstream.rs:17-54` same pattern, `method_root` | Done | — |
| f | inbound shapes | Gateway maps Chat/Responses/Messages/Gemini → envelope (`codeassist.go`) | Upstream `GENERATIVE_LANGUAGE`; gateway codec to IR | Done | — |
| g | streaming | SSE `streamGenerateContent` | `wire::execute` streaming | Done | — |
| h | tool calls | Gateway plain schema / function declarations (`codeassist.go`) | `plain_code_assist_tools` in upstream rewrite (`upstream.rs:48-49`) | Partial | Full tool parity vs gateway tests unverified |
| i | images | Generative Language parts | via IR content parts | Partial | unverified |
| j | reasoning / thinking | Thought signatures, skip validator (`codeassist.go:29-31`) | unverified in gemini plugin | Partial | Thought signature parity (R-49) |
| k | quota / usage | `retrieveUserQuota` per model windows | `quota.rs` buckets from same RPC; R-49 applies the experiment-driven quota-id mapping | Done | — |
| l | model list | `retrieveUserQuota` preview signal + `listExperiments` flags choose Gemini CLI 0.61's catalog (`google.go:854-999`, `1180-1236`); `fetchAvailableModels` is Antigravity's path | `models.rs` ports the flag ids, catalog filtering, display names, and image flags; hourly per-instance cache; offline reference cases | Done | — |
| m | multi-account | Multiple google logins | multiple accounts | Done | — |
| n | error classification | RetryInfo → retry-after (`codeassist.go`) | `wire` retry_delay in feature-parity doc | Partial | Individual-account refusal persistence (`google.go:330+`) |
| o | daily check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | read gemini home only in reference docs | no `AgentSignIn` on gemini spec | Missing | Optional discover of `~/.gemini/oauth_creds.json` (R-49) |
| q | extras | Code Assist Standard/Enterprise project onboarding; `SetGoogleProject` names or clears an account's stored Cloud project; model-name mapping from the reference catalog | `project.rs` loadCodeAssist/onboardUser and leased account updates; `hofund accounts project gemini <email> <gcp-project-id>` trims, stores, and clears the project used by execute and quota; `models.rs` maps listed, outgoing, and quota ids from the same experiments; offline tests only | Done | — |

**Gaps**

- Google account import formats — **M**, **R-49**.
- Gateway-grade tool/thought-signature edge cases — **M**, **R-49**.

---

## Google Antigravity

**Reference files read:** `google.go:82-100` (antigravity app, daily vs prod base), `antigravity_models.go`, `antigravity_pool.go`, `google_import.go`, `codeassist.go` (antigravity cloak, effort variants).

**Community:** `packages/antigravity-cloak/` (middleware for turned-away prompts).

**hofund path:** `plugins/subscriptions/antigravity/src/`; the existing desktop adapter remains separate under `plugins/agents/antigravity/`.

| # | capability | reference v0.1.3005 | hofund R-49c | status | gap to close |
|---|------------|----------------------|--------------|--------|--------------|
| a1 | sign-in: loopback OAuth (Antigravity client) | `google.go:82-90` embeds Antigravity's installed-app client, `/oauth-callback`, `localhost` | `sign_in.rs` and `lib.rs`: same scopes, PKCE, localhost path; client from plugin `client_id`/`client_secret` or `HOFUND_ANTIGRAVITY_CLIENT_*` env (hofund does not ship the 3005 literals). Offline tests use a fake client (`r49c_oauth_*`) | Done | Operator must supply the installed-app client; 3005 embeds it |
| b | import sources | `google_import.go` refresh token imports | none | Missing | Antigravity Cockpit/Manager/CLIProxyAPI export import (R-49 follow-up) |
| c | token storage | logins.json + in-memory refresh | Account store keeps the refresh token; `tokens.rs` keeps access tokens in memory | Done | — |
| d | refresh | same as Google subscriptions | `tokens.rs`: oauth2.googleapis.com, two-minute margin, lapsed grants; offline refresh/refusal tests | Done | — |
| e | upstream wire | `daily-cloudcode-pa.googleapis.com` + prod load base (`google.go:98-100`) | `Endpoints`, `project.rs`, and `upstream.rs` use those bases and the Antigravity Code Assist envelope | Done | — |
| f | inbound shapes | Same Code Assist envelope; effort variant IDs (`antigravity_models.go`) | SDK codecs translate inbound wires; `models.rs` folds families and `upstream.rs` picks effort variants; offline unary test | Done | — |
| g | streaming | SSE envelope | `upstream.rs` lifts each `response` frame on `streamGenerateContent?alt=sse`; offline stream test | Done | — |
| h | tools | tool declarations and thought signatures (`internal/gateway/codeassist.go`) | Standard IR tools plus `plain_code_assist_tools`; no Antigravity-specific full gateway equivalence run | Partial | Thought-signature and tool-history edge cases remain unverified |
| i | images | inline image parts on the Code Assist wire (`internal/gateway/codeassist.go`) | Standard IR image parts and model image flags | Partial | Offline image-request fixture remains |
| j | reasoning | thought signatures / thinking config, cloak and turned-away detection (`codeassist.go`, `packages/antigravity-cloak`) | Effort ids/levels, reference prompt cloak, and non-cooling 400 mapping are tested offline; thought-signature edges are not | Partial | Thought-signature parity and live verification remain |
| k | quota / usage | `fetchAvailableModels` model quota plus `retrieveUserQuotaSummary` shared windows | `quota.rs` reads both, including reset times and omitted-fraction exhaustion; offline test | Done | — |
| l | model list | `antigravity_pool.go:15-49` union across accounts; effort folding (`antigravity_models.go`) | `models.rs` parses picker order, filters hidden/disabled ids, falls back through tiers, folds families, and merges account unions; offline tests | Done | — |
| m | multi-account | Pool catalog union | One Google email has distinct Gemini and Antigravity account ids; all stored Antigravity accounts join the union in account order | Done | — |
| n | error classification | 429 “turned away” prompts (`codeassist.go:51-66`) | `upstream.rs` distinguishes prompt refusal from real quota and returns a non-cooling 400 with the reference hint | Done | — |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | Antigravity desktop agent separate | Desktop `agent.toml` only; no subscription sign-in switch | Missing | Agent sign-in switching is not in R-49c |
| q | extras | Ban-risk warning in UI; version manifest URL (`google.go:68`) | Full account-suspension warning prints before CLI sign-in and remains in docs; request version is the fixed reference fallback | Partial | Dynamic version-manifest refresh remains |

**Gaps**

- Refresh-token export import and agent sign-in switching — **M**, R-49 follow-up.
- Full tool/image/thought-signature gateway fixtures and dynamic app-version refresh — **M**, R-49 follow-up.
- Real-account verification is not performed in this slice.

---

## SuperGrok (`grok`)

**Reference files read:** `grok.go`, `grok_accounts.go`, `grok_install.go`, `grok_usage.go`, `migrate_side.go`.

**Community plugin:** `packages/grok/index.mjs` (CLI login, signing, refresh via `grok models`).

**hofund path:** `plugins/subscriptions/grok/`.

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: CLI device (`grok login`) | `grok.go:13-17`, plugin `index.mjs:1-9` | `lib.rs:3-7`, bridge flow runs `grok login --device-auth` | Done | — |
| a2 | browser / API key / paste | device via CLI only | N/A | N/A | — |
| b | import sources | Read CLI `auth.json`; reference-owned homes (`index.mjs:48-56`) | `discover.rs`, `creds.rs` import paths | Done | — |
| c | token storage | CLI home + plugin store | account secret + CLI home path in blob | Done | — |
| d | refresh | CLI `grok models` renew (`index.mjs:6-7`, `refresh.test.mjs`) | `refresh.rs:16-29` same | Partial | Serialized renew vs concurrent callers (R-24 / R-50) |
| e | upstream wire | `https://cli-chat-proxy.grok.com/v1` OpenAI Responses + signed headers | `lib.rs:64-66`, `upstream.rs:73-80` | Done | — |
| f | inbound shapes | Responses upstream; body rewrite in plugin | `ProtocolId::OPENAI_RESPONSES`, `shape.rs` | Done | — |
| g | streaming | SSE | wire streaming | Done | — |
| h | tool calls | Allowed tool types set (`index.mjs:33-35`) | `shape.rs` tool filtering | Partial | Full tool type set parity (R-50) |
| i | images | `input_image` in shape tests | `shape.rs:147-149` | Done | — |
| j | reasoning | reasoning items in Responses | `shape.rs:106+` | Partial | unverified vs xAI backend |
| k | quota / usage | plugin usage hooks; legacy `grok_usage.go` | `quota.rs` allowance windows | Partial | Deferred usage / step billing if in plugin (R-50) |
| l | model list | CLI/backend model API | `models.rs` | Done | — |
| m | multi-account | reference-owned homes per extra account | multiple accounts | Done | — |
| n | error classification | auth errors → NeedsSignIn | unverified | Partial | R-50 |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | read `GROK_HOME` | `lib.rs:46` AgentSignIn, discover/import | Done | — |
| q | extras | CLI install helper (`grok_install.go`, `index.mjs:26-31`) | `cli.rs:29` error if missing only | Partial | Install guidance / GROK_BIN_DIR parity (R-50) |

**Gaps**

- CLI install discovery helper like the reference — **S**, **R-50**.
- Renew serialization under concurrent refresh — **S**, **R-24**, **R-50**.
- Tool-type allowlist and x_search config parity — **M**, **R-50**.

---

## Qoder (international + Qoder CN)

**Reference files read:** `qoder.go`, `migrate_qoder.go`, `qoder_checkin.go`; plugin `packages/qoder/index.mjs` (COSY SSE, device PKCE, VPC enterprise, two provider ids `qoder` / `qoder-cn`).

**hofund path:** `plugins/subscriptions/qoder/` (single plugin id `qoder`, site in credential).

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: device / PKCE | plugin `index.mjs:6-10` desktop PKCE device flow | `lib.rs:145-157` import-only; unsupported native sign-in | Missing | Device PKCE sign-in (R-54) |
| a2 | browser OAuth | qoder.com page in plugin | none | Missing | R-54 |
| a3 | API key / paste | import credential JSON | `import.rs:7-17` user paste import | Partial | — |
| a4 | agent credential discovery | plugin reads agent `credential.json` | none | Missing | Agent home discover (R-54) |
| b | import sources | legacy `qoder.json` migrate (`qoder.go:47-74`) | JSON import only | Partial | Legacy file migrate (R-54) |
| c | token storage | plugin-owned; job + refresh tokens | `creds.rs` SecretObject | Done | — |
| d | refresh | job token refresh; single-use refresh token (`index.mjs:9-10`) | `refresh.rs:16-46` lease + forced renew on 401 | Partial | `RELOGIN_GAP` / renew serialization (R-54) |
| e | upstream wire | SSE `agent_chat_generation` + COSY envelope (`index.mjs:45-54`) | OpenAI Chat `.../chat/completions` (`upstream.rs:43-47`) | Missing | COSY SSE wire (R-54) |
| f | inbound shapes | OpenAI chat in IR → Qoder codec in plugin | Chat completions passthrough | Missing | R-54 |
| g | streaming | COSY SSE `agent_chat_generation` (`packages/qoder/index.mjs:13-18`, `CHAT_PATH` line 45) | OpenAI Chat to `{base}/chat/completions` (`upstream.rs:43-54`) | Missing | COSY SSE stream (**R-54**) |
| h | tool calls | XML-in-text lifted in plugin | unverified on hofund path | Missing | R-54 |
| i | images | plugin tests | unverified | Partial | R-54 |
| j | reasoning | model config in plugin | unverified | Partial | R-54 |
| k | quota / usage | openapi account-context; plugin usage | `quota.rs` via openapi URL (`lib.rs:218-221`) | Partial | No silent fallback vs plugin |
| l | model list | `/algo/api/v2/model/list` with device headers in plugin | `/api/v1/models` openapi (`models.rs:19-21`) | Partial | COSY/device headers on list (R-54) |
| m | multi-account | separate subscriptions `qoder` and `qoder-cn` (`qoder.go:28-31`) | one plugin, `Site` in creds (`creds.rs:10-38`) | Partial | Split config ids or document unified id (R-54) |
| n | error classification | lapsed / NeedsSignIn in plugin | 401 → refresh (`upstream.rs:68-71`) | Partial | NeedsSignIn on 400 refresh (R-25 #612) |
| o | daily check-in | `qoder_checkin.go` + plugin campaigns | `checkin.rs` + `with_daily_checkin` | Partial | 30-minute recheck for `qoder`, Qoder CN and `zcode` (`plugin_checkin.go:274` `pluginRechecks`; interval `wbCheckinEvery` `workbuddy_checkin.go:55`, used in `checkin.go:110-117`) (R-54) |
| p | agent sign-in | plugin device identity | none | Missing | R-54 |
| q | extras | Qoder CN VPC instance hosts (`index.mjs:29-36`); COSY_VERSION desktop | CN/global endpoints only (`lib.rs:37-71`) | Missing | VPC enterprise sign-in (R-54) |

**Gaps**

- Replace Chat Completions stub with COSY SSE upstream — **L**, **R-54**.
- Native device PKCE sign-in + agent credential discover — **L**, **R-54**.
- Qoder CN VPC instance URLs — **M**, **R-54**.
- Refresh renew serialization (**R-54**) + NeedsSignIn mapping (**R-25** `#612`) — **S**.

---

## CodeBuddy / WorkBuddy (CN + international)

**Reference files read:** `workbuddy.go`, `workbuddy_checkin.go`, `migrate_workbuddy.go`.

**Community plugin:** `packages/workbuddy/index.mjs` (browser sign-in, desktop auth files, `/v2` chat, restriction code 11140).

**hofund path:** `plugins/subscriptions/workbuddy/`.

| # | capability | reference v0.1.3005 | hofund R-55 | status | gap to close |
|---|------------|----------------------|-------------|--------|--------------|
| a1 | sign-in: browser (Tencent page poll) | plugin `index.mjs:5-7`, state/token/account polling | `sign_in.rs` starts the returned HTTPS page, then polls token and account state for either `--site` (`r55_browser_sign_in_works_for_both_sites`, mocks only) | Done | — |
| a2 | sign-in: API key | API-key accounts (`packages/workbuddy/index.mjs:404-418` `keyAccount`, `apikey.test.mjs`) | `sign_in.rs::import_api_key` stores a key account with the anonymous suffix; chat sends bearer plus `X-API-Key`, never `X-Domain`; key quota and check-in make no calls (`r55_api_key_sign_in_works_for_both_sites_without_exposing_key`) | Done | — |
| a3 | desktop login reuse | read `workbuddy-desktop.info` / AI variant | `discover.rs:14-31`, `auth_store.rs` | Done | — |
| b | import sources | desktop JSON under CodeBuddyExtension data | same paths in discover | Done | — |
| c | token storage | plain JSON on disk (plugin) | encrypted hofund secret + desktop path in blob | Done | — |
| d | refresh | desktop token re-read + OAuth refresh in plugin | `refresh.rs` ensure_fresh / desktop re-read; encrypted access or refresh objects fail closed with a secret-free browser-sign-in error (`r55_desktop_rejects_encrypted_tokens_without_exposing_envelopes`) | Done | No decryptor exists in the frozen reference; none is invented |
| e | upstream wire | `{endpoint}/v2` streamed chat, gzipped body with `Content-Encoding: gzip` (`index.mjs:7-9`, `1502-1519`) | plain JSON to `{base}/v1/chat/completions` (`upstream.rs:50-51`), no gzip | Partial | `/v2` path + gzip bodies remain |
| f | inbound shapes | OpenAI chat + WorkBuddy headers | OpenAI chat + `creds::signed_headers` and `chat_headers` | Done | — |
| g | streaming | stream only in plugin | wire streaming | Done | — |
| h | tools | `fitTools`, `tools.test.mjs`, `aftertools.test.mjs` | unverified | unverified | Confirm in a follow-up |
| i | images | `/v2/images/*`, `images.test.mjs`, `declared-images.test.mjs` | unverified | unverified | Confirm in a follow-up |
| j | reasoning | effort lists, `reasoning.test.mjs`, `efforts.test.mjs` | unverified | unverified | Confirm in a follow-up |
| k | quota / usage | profile usage page + billing API | `quota.rs` billing without guessed totals; key accounts return no windows without calling billing | Partial | Profile-page usage remains |
| l | model list | `/v3/config` in plugin | `models.rs` config fetch with the signed client headers | Done | — |
| m | multi-account | `workbuddy` vs `workbuddy-ai` provider ids | single plugin, `site::Site` CN vs International (`lib.rs:31-35`) | Partial | Separate subscription ids if needed for routing labels |
| n | error classification | `WBRestricted` code 11140 (`workbuddy.go:267-299`) | `errors.rs` accepts top-level, `data`, or `error` numeric/string codes, appends the exact hint, and rests the account for two hours (`r55_restriction_*`, upstream test) | Done | — |
| o | daily check-in | `workbuddy_checkin.go` + plugin; CN only | `checkin.rs`, `with_daily_checkin`; international and key accounts are inactive | Done | — |
| p | agent sign-in | desktop auth paths | `AgentSignIn` workbuddy + workbuddy-ai (`lib.rs:31-35`) | Done | — |
| q | extras | User-Agent must be WorkBuddy/version (`index.mjs:8-9`) | `creds.rs` sends the reference's 5.9.3 app/2.165.0 CLI identity, attribution and chat SDK headers per site (`r55_signed_headers_match_each_site`, `r55_chat_headers_match_the_desktop_cli`) | Done | — |

**R-55 completed**

- Browser and API-key sign-in work for both sites through the existing subscription proof flow, with deterministic recorded-HTTP tests only.
- Chat, models, quota, check-in, and refresh use the reference's client identity and applicable signing headers; key material is sensitive and never included in errors.
- Code 11140 produces the reference hint and a two-hour account rest.
- Encrypted desktop tokens remain unreadable by design and direct the user to browser sign-in without rendering the encrypted envelope.

**Remaining gaps**

- `/v2` chat path and gzip request bodies.
- Tool, image, and reasoning parity is still unverified.
- Profile-page usage and separate subscription ids remain as listed above.

---

## Z Code (`zcode`)

**Reference files read:** `zcode.go`, `zcode_device.go`, `zcode_team.go`, `migrate_zcode.go`, `plugin_checkin.go`, `workbuddy.go`, `internal/agent/zcode.go`, and `internal/appdir/zcode.go`. The earlier audit cited plugin `packages/zcode/index.mjs` and `README.md`, but that package is absent from the frozen tag; plugin-only wire details not also visible in Go remain unverified.

**hofund path:** `plugins/subscriptions/zcode/`.

| # | capability | reference v0.1.3005 | hofund 08035c9 + R-57 | status | gap to close |
|---|------------|----------------------|-----------------------|--------|--------------|
| a1 | sign-in: device / portal OAuth | plugin sign-in flows in `index.mjs` | `lib.rs` import/discover only | Partial | Full portal sign-in; the flow is not fully visible in the frozen Go sources (**R-57b**) |
| a2 | API key import | plugin supports key plans | credential import via agent store | Partial | — |
| a3 | paste / browser | plugin OAuth | unsupported in `start_sign_in` | Missing | **R-57b**; no guessed OAuth or paste wire |
| b | import sources | encrypted `~/.zcode/v2/credentials.json` (`zcode.go:14-18`) | `creds.rs` decrypt + `discover.rs` | Done | — |
| c | token storage | logins.json for reference-added; encrypted agent store read | SecretObject + device mid (`creds.rs`) | Done | — |
| d | refresh | JWT refresh near expiry in plugin | `refresh.rs` re-imports through the account lease, preserves `lapsed` for the same team token, clears it for a rotated token, and rejects non-absolute credential paths (`r57_*` tests) | Partial | Plugin JWT renewal rules remain unverified (**R-57b**) |
| e | upstream wire | Anthropic plan API bases (`zcode.go:47-48`); Start Plan accounts go to `START_BASE` (`zcode/index.mjs:33`, used at `:2371`) | `upstream.rs` Messages on plan base; JWT-only Start Plan import refused | Partial | Start Plan routing and its required request transformations; the base is visible but the full plugin wire is not (**R-57b**) |
| f | inbound shapes | Messages only upstream | `upstream.rs` rejects non-Messages | Done | — |
| g | streaming | Anthropic SSE | wire streaming | Done | — |
| h | tool calls | Anthropic tools | standard Messages wire | Partial | unverified |
| i | images | per-model `supportsImage` (`packages/zcode/index.mjs:1690`) | no verified per-model mapping | unverified | Plugin catalog shape is absent from the frozen tag (**R-57b**) |
| j | reasoning | per-model reasoning levels mapped to thinking budgets (`packages/zcode/index.mjs:1691`, `1753-1765`) | no verified per-model mapping | unverified | Plugin levels and budgets are absent from the frozen tag (**R-57b**) |
| k | quota / usage | monitor `/api/monitor/usage/quota/limit` personal/team | `quota.rs` monitor endpoints | Done | — |
| l | model list | portal + plan APIs in plugin | `models.rs` | Partial | Gift/check-in tied models unverified (**R-57b**) |
| m | multi-account | team seats from `setting.json` (`zcode_team.go`); plugin 0.1.20 labels a team account `<email> (team)` (`migrate_zcode.go`) | `team.rs` project keys and separate `(team)` labels (`r57_team_accounts_have_a_separate_label`) | Done | — |
| n | error classification | ZCodeRestricted hint code 3012 (`workbuddy.go`); the account rests for two hours (`zcode_blocked_test.go`) | `http_auth.rs` keeps HTTP 405, adds the risk-control/support hint and two-hour retry hint (`r57_3012_has_a_risk_control_hint_and_cooldown`) | Done | — |
| o | daily check-in | plugin `auth.checkin` gift plans (`index.mjs:2740+`); 30-minute recheck (`plugin_checkin.go`) | no check-in in zcode plugin | Missing | Claim endpoints, payloads, and result mapping are not fully visible in the frozen Go sources, so neither claims nor recheck scheduling are guessed (**R-57b**) |
| p | agent sign-in switch | agent switching into Z Code config; team keys are created when absent (`zcode_team.go` comments) | `agent_sign_in.rs` refuses credential switching; separate team labels implemented | Partial | Agent config switching and missing-key creation request body remain unverified (**R-57b**) |
| q | extras | `X-Device-Mid`; plugin pins `APP_VERSION` 3.17.2 for User-Agent and `X-ZCode-App-Version`; core `zcode.go` still says 3.14.3 | `device.rs`; `APP_VERSION` 3.14.3 in `lib.rs` | Partial | Send 3.17.2 in User-Agent and `X-ZCode-App-Version` (**R-57b**) |

**R-57 completed**

- Kept `lapsed` across same-secret team refresh re-imports, serialized re-import through the account lease, failed closed on relative credential paths, and added separate team labels (#613 follow-ups).
- Mapped code 3012 to the reference's user-facing risk-control hint and two-hour cooldown.

**Gaps (R-57b)**

- Daily marketing/check-in gift flow and its 30-minute recheck — **M**.
- Native portal sign-in (not import-only) — **M**.
- Agent switching and creating a missing team key — **M**.
- Start Plan routing and request shaping — **M**.
- Per-model image and reasoning mappings — **M**.
- Send app version 3.17.2 in User-Agent and `X-ZCode-App-Version` — **S**.

---
