# Subscription parity checklist (R-46)

Per-vendor, code-level parity audit of hofund subscription plugins against the frozen reference baseline **v0.1.3005** (tag commit 6abacd46; Go core under `internal/provider`, `internal/gateway`, `internal/claudebridge`) and the community auth plugins it ships with (the reference's community plugins repository at 1ad712d, the last commit before the v0.1.3005 tag). hofund side: main at **08035c9**.

Scope: the 14 subscriptions in the v0.2.0 plan (Claude Pro/Max, Cursor, Devin, Zed, ChatGPT/Codex plus ChatGPT API, Copilot plus GHE, Gemini Code Assist plus Antigravity, SuperGrok, Kiro, Factory, Cline, Qoder plus CN, CodeBuddy/WorkBuddy plus international, Z Code). Each section covers rows a-q: sign-in methods, import, token storage, refresh, upstream wire, inbound API shapes, streaming, tools, images, reasoning, quota, model list, multi-account, error classification, warm-up/check-in, agent sign-in switching, vendor extras.

This is a code-level checklist. Real-account smoke tests are out of scope and happen after the release candidate. Rows marked "unverified" are places where the audit could not confirm behaviour from code alone; the implementing task must confirm them.

Legend: **Done** = matches the reference at checklist granularity; **Partial** = subset, or mocks only; **Missing** = not implemented; **N/A** = the reference does not offer it.

## Summary

| Vendor section | Done | Partial | Missing | N/A | Unverified |
|---|---|---|---|---|---|
| Claude Pro/Max (`claude`) | 1 | 11 | 9 | 2 | 0 |
| Cursor (`cursor`) | 1 | 11 | 7 | 2 | 0 |
| Devin (`devin`) | 2 | 5 | 11 | 1 | 0 |
| Zed (`zed`) | 0 | 11 | 5 | 3 | 0 |
| Kiro (`kiro`) | 0 | 13 | 5 | 1 | 0 |
| Factory / Droid (`factory`) | 1 | 15 | 0 | 3 | 0 |
| Cline / ClinePass (`cline`) | 2 | 12 | 1 | 3 | 1 |
| ChatGPT / Codex (`codex` backend) and ChatGPT API (`chatgpt-api`) | 8 | 11 | 2 | 0 | 0 |
| GitHub Copilot (github.com and GHE) | 10 | 5 | 2 | 5 | 0 |
| Gemini CLI (Code Assist) | 8 | 6 | 2 | 2 | 0 |
| Google Antigravity | 0 | 0 | 16 | 1 | 0 |
| SuperGrok (`grok`) | 10 | 6 | 0 | 2 | 0 |
| Qoder (international + Qoder CN) | 1 | 10 | 9 | 0 | 0 |
| CodeBuddy / WorkBuddy (CN + international) | 7 | 6 | 3 | 0 | 3 |
| Z Code (`zcode`) | 6 | 9 | 2 | 0 | 2 |

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

**Hofund path:** `plugins/subscriptions/claude/src/` (`lib.rs`, `cli.rs`, `creds.rs`, `account.rs`, `agent_sign_in.rs`, `discover.rs`, `import.rs`, `lock.rs`).

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
| e | upstream request wire | No direct Anthropic HTTP; subprocess + MCP bridge (`claude_subscription.go:3-29`, `claudebridge/mcp.go:54-59`) | `upstream()` returns unsupported (`lib.rs:427-434`) | Missing | MCP bridge + long-lived runs (`claude_subscription.go:74-79`) |
| f | inbound API shapes | Gateway `serveClaudeSubscription` for Anthropic Messages and translated Chat/Responses paths (`gateway.go:4995`, `claude_subscription.go:6041+`) | Not served | Missing | Chat Completions, Responses, Messages → bridge (`R-6`) |
| g | streaming | Bridge streams Claude Code stdout/SSE to caller (`claude_subscription.go` run loop) | Not served | Missing | Stream bridge events to IR |
| h | tool calls | MCP `tools/call` blocks; results on next HTTP request (`claude_subscription.go:9-13`, `mcp.go:36-48`) | Not served | Missing | Tool parking protocol (`R-6`) |
| i | images | Gateway image tests pass through bridge (`claude_images_test.go` cited in tree) | Not served | Missing | Image blocks via bridge |
| j | reasoning / thinking | Effort / think-off **staged** in gateway (`6158-6161`, `6072-6076`, `anthropic.go:281-291`) | Not served | Missing | Effort and extended thinking via CLI; hofund uses **`X-Hofund-Cache-Marks`** (neutral header) instead of reference vendor-prefixed cache opt-in header (`claude_cache.go:36-38`) |
| k | quota / usage | `claude -p /usage` parse (`claude_usage_cli.go:50-57`); plus `NoteClaudeLimits` from stream headers (`claude_limits.go:69-71`) | `quota()` unsupported (`lib.rs:407-414`) | Missing | `/usage` CLI + header-derived windows (`R-5`) |
| l | model list | From Claude Code / catalog in gateway context (unverified detail); usage scopes per model in limits | `list_models()` unsupported (`lib.rs:417-424`) | Missing | Model list + scoped week windows (`R-5`) |
| m | multi-account | Multiple saved logins, routing, agent switch (`claude_routed_signin_test.go` etc.) | Daemon multi-account + agent switch with locks | Partial | Quota-aware routing needs row k |
| n | error classification | `ClaudeSignInRequired`, refusal vs retry (`claude_auth.go`); subscription refusals move account (`gateway/refusal_accounts_test.go`) | Refresh/auth errors only; no upstream | Partial | Rest/cooldown from bridge refusals when upstream exists |
| o | daily warmup | `KeepClaudeWindowsWarm` / `claude_warmup.go:77-79` | `daily_warmup` on spec + `account::warmup` (`lib.rs:54-63`, `454-459`) | Partial | Gateway-coordinated warm request path (probe) vs CLI-only warm-up |
| p | agent sign-in switching | Switch with locks, CAS on refresh token (`logins.go` patterns; docs in subscriptions.md) | `write_agent_sign_in`, discover/import (`lib.rs:437-475`) | Partial | Full lock parity documented in subscriptions.md gaps (overage quota) |
| q | vendor extras | WSL Claude, per-login proxy (`claude_signin.go:139-145`), setup-token inference-only | Proxy/WSL/setup-token absent | Partial | Proxy env for sign-in and runs; setup-token (`R-5`, `R-6`) |

### Gaps (Claude)

- MCP subscription bridge and multi-protocol gateway translation — **L**, **R-6**
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
| e | upstream wire | Connect bidi `AgentService/Run` + HTTP/1 RunSSE + BidiAppend (`cursor/index.mjs:13-17`, `44-46`) | `upstream()` unsupported (`lib.rs:365-372`) | Missing | Full Connect session (**R-8**, **R-9**) |
| f | inbound shapes | OpenAI-compatible chat to plugin fetch (`cursor/index.mjs:29-30`) | Not served | Missing | Map Chat/Responses/Messages to Cursor wire (**R-8**, **R-9**) |
| g | streaming | Connect stream / RunSSE (`cursor/index.mjs:13-17`) | Not served | Missing | **R-8** |
| h | tool calls | MCP `CallDynamicTool` (`cursor/index.mjs:21-26`, `51`) | Not served | Missing | **R-9** |
| i | images | unverified in core; plugin native session handles blobs | Not served | Missing | **R-9** |
| j | reasoning | Plugin tests `reasoning.test.mjs`; max mode | Not served | Missing | **R-9** |
| k | quota / usage | Connect `GetCurrentPeriodUsage`, team header (`cursor/index.mjs:171-176`, `connect.rs` parity) | `quota()` via Connect unary (`lib.rs:306-322`, `connect.rs:9-38`) | Partial | Deferred/step usage, credit events (`cursor/deferred-usage.test.mjs`) — **R-9** |
| l | model list | Plugin catalog + agent list; core `cursor_models.go` contexts | `cursor-agent models` parse + family fold (`models.rs`, `lib.rs:337-362`) | Partial | Server-side catalog merge like plugin (`catalog.test.mjs`) |
| m | multi-account | Team id per token (`activeTeamId`) | Multiple accounts; team header not evident in hofund Connect calls | Partial | `x-cursor-team-id` on chat + usage (**R-8**) |
| n | error classification | Plugin maps budget/refusal (`errors.test.mjs`) | Connect HTTP errors only | Partial | Map to rest/refusal (**R-9**) |
| o | warmup / keepalive | none found (no keepalive in the Cursor core or its auth plugin) | None | N/A | — |
| p | agent sign-in | Core `cursorStatus` mark (`cursor.go:59-80`) | discover/import (`lib.rs:383-396`) | Done | — |
| q | extras | HTTP/2 fallback, ghost mode, client version string (`cursor/index.mjs:48`, `connect.rs:24-26`) | Partial headers on dashboard only | Partial | Agent API headers + HTTP/1 fallback (**R-8**) |

### Gaps (Cursor)

- Connect RunSSE chat upstream — **L**, **R-8**, **R-9**
- Tools, reasoning, images, usage detail — **L**, **R-9**
- CURSOR_API_KEY exchange sign-in — **M**, **R-8**
- Team billing header — **S**, **R-8**

---

## Devin (`devin`)

**Reference files read:** `internal/provider/devin.go`, `devin_accounts.go`, `devin_images.go`; `packages/devin/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/devin/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: CLI PKCE | Plugin: loopback PKCE via `app.devin.ai` (`devin/index.mjs:28-31`, `SIGN_IN_TIMEOUT`) | Bridge flow imports existing CLI creds only (`lib.rs:105-127`) | Partial | Interactive PKCE sign-in in plugin — not in hofund |
| a2 | sign-in: reuse CLI | Read `credentials.toml` (`devin.go:54-67`) | `creds.rs` + discover | Done | — |
| a3 | API key / device | N/A | N/A | N/A | — |
| b | import | Side login mover + CLI path | XDG `credentials.toml` discover/import | Done | — |
| c | token storage | Plugin + TOML on disk | Daemon secret; atomic agent write on switch | Partial | — |
| d | refresh | Plugin renews tokens (unverified); core reads CLI | `refresh` loads stored only (`lib.rs:174-176`) | Partial | OAuth refresh if CLI tokens expire (**R-12**) |
| e | upstream wire | Protobuf Connect `GetChatMessage` on `server.codeium.com` (`devin/index.mjs:28-32`) | `upstream.rs` explicitly refuses (`devin/upstream.rs:20-35`) | Missing | Protobuf chat RPC (**R-12**, **R-13**) |
| f | inbound shapes | Chat completions + Anthropic for Claude models (`devin/index.mjs:19-26`) | Not served | Missing | Dual npm routing (**R-12**) |
| g | streaming | Connect stream from GetChatMessage | Not served | Missing | **R-12** |
| h | tool calls | Plugin `tool-ids.test.mjs`, split calls | Not served | Missing | **R-13** |
| i | images | Plugin `images.test.mjs`, `devin_images.go` in core catalog | Not served | Missing | **R-13** |
| j | reasoning / thinking | `thinking-signature.test.mjs`, effort variants | Not served | Missing | **R-13** |
| k | quota / usage | Plugin `usage.test.mjs` | `quota()` empty vec (`lib.rs:179-185`) | Missing | Usage RPC (**R-13**) |
| l | model list | CLI JSON + `GetCliModelConfigs` (`devin/index.mjs:44-134`) | `list_models` refuses offline CLI (`lib.rs:187-195`) | Missing | Live model list (**R-12**) |
| m | multi-account | EU host `eu.codeium.com` (plugin) | unverified in hofund | Partial | Region selection (**R-12**) |
| n | error classification | Plugin `errors.test.mjs` | Not served | Missing | **R-13** |
| o | keepalive | `keepalive.test.mjs` | None | Missing | **R-13** (new to the plan in this audit) |
| p | agent sign-in | Core installs CLI (`devin.go:8-12`) | `agent_sign_in.rs` write TOML | Partial | CLI install helper like core (`DevinExecutable`) |
| q | extras | gzip request, identity headers, CLI version pin (`devin/index.mjs:33-36`) | Not served | Missing | **R-12** |

### Gaps (Devin)

- GetChatMessage upstream (protobuf, streaming) — **L**, **R-12**, **R-13**
- Models list from CLI/API — **M**, **R-12**
- Tools, images, reasoning, usage — **L**, **R-13**
- PKCE sign-in (not only import) — **M**, **R-12**
- CLI install/bootstrap — **S**, **R-12** (new to the plan in this audit)
- Keepalive (`keepalive.test.mjs`) — **S**, **R-13** (new to the plan in this audit)

---

## Zed (`zed`)

**Reference files read:** `internal/provider/zed.go`, `zed_body.go`, `migrate_zed.go`; `packages/zed/index.mjs`, `README.md`.

**Hofund path:** `plugins/subscriptions/zed/src/`.

| # | capability | reference v0.1.3005 behaviour | hofund 08035c9 | status | gap to close |
|---|------------|------------------------------|----------------|--------|--------------|
| a1 | sign-in: native app RSA loopback | Plugin opens `zed.dev/native_app_signin` (`zed/index.mjs:8-13`) | `start_sign_in` unsupported — import only (`lib.rs:113-120`) | Missing | Native sign-in flow (**R-56**, **R-16**) |
| a2 | sign-in: import saved creds | Core `zedCreds` mover (`zed.go:14-34`) | `import.rs` + `TokenFile` flow | Partial | — |
| a3 | API key / device | N/A | N/A | N/A | — |
| b | import | Built-in side logins | `hofund subscription import` | Partial | — |
| c | token storage | Plugin holds user id + access + system id | `creds.rs` stored secret | Partial | — |
| d | refresh | POST `/client/llm_tokens` on 401 (`zed/index.mjs:14-16`) | `refresh.rs` against cloud API | Partial | Outdated-token headers (`x-zed-expired-token`) |
| e | upstream wire | POST `/completions` NDJSON envelope (`zed/index.mjs:17-20`) | `upstream.rs` unsupported (`zed/upstream.rs:28-38`) | Missing | Completions stream (**R-16**) |
| f | inbound shapes | Per-provider SDK body in `provider_request` (`zed/index.mjs:22-25`) | `body.rs` shapes Responses maps only (offline) | Partial | Wire completions + unwrap stream (**R-16**) |
| g | streaming | NDJSON `event` lines | Not served | Missing | **R-16** |
| h | tool calls | Provider-native in envelope | Body transform only | Partial | End-to-end with upstream |
| i | images | Model flags `supports_images` (`zed/index.mjs:57-62`) | unverified | Missing | **R-16** |
| j | thinking | `supports_thinking` in catalog | `body.rs` reasoning item tweaks | Partial | **R-16** |
| k | quota / usage | Plan limits via Zed cloud (plugin); core reader minimal | `quota()` empty (`lib.rs:162-167`) | Missing | Usage/plan endpoints (**R-16**) |
| l | model list | GET `/models` (`zed/index.mjs:17-18`) | `models.rs` from cloud | Partial | Snapshot fallback vs live merge |
| m | multi-account | Org id on creds (`zed.go:21-22`) | Stored org field | Partial | Org-scoped requests |
| n | error classification | `SignInExpired`, 402 plan (`zed/index.mjs:83-147`) | Refresh auth errors | Partial | Lapsed vs retry (**R-16**) |
| o | warmup | N/A | N/A | N/A | — |
| p | agent sign-in | N/A (Zed editor sign-in, not agent file) | No agent discover | N/A | — |
| q | extras | User-Agent `Zed/1.23.0` (`zed/index.mjs:35-69`) | unverified on refresh | Partial | Client version parity (**R-16**) |

### Gaps (Zed)

- Native app sign-in (RSA loopback) — **L**, **R-56**
- Cloud `/completions` upstream + streaming — **L**, **R-16**
- Quota/plan display — **M**, **R-16**
- Token stale/outdated handling — **S**, **R-16**

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
| e | upstream wire | Event-stream `generateAssistantResponse` (`kiro/index.mjs:16-18`; `upstream.rs:44-48`) | Same endpoint, unary decode only | Partial | Streaming request/response (**R-51**) |
| f | inbound shapes | Anthropic Messages only (`kiro/index.mjs:14-18`) | Messages only (`upstream.rs:109-112`) | Partial | Chat/Responses N/A |
| g | streaming | AWS event stream to Messages SSE | `kiro_body.rs` rejects `stream: true` (`kiro_body.rs:9-11`) | Missing | Stream decode to IR (**R-51**) |
| h | tool calls | Plugin tool tests (`tool-names.test.mjs`) | Rejected in `kiro_body.rs` | Missing | **R-51** |
| i | images | `image-size.test.mjs` | Rejected text-only | Missing | **R-51** |
| j | reasoning / effort | `effort.test.mjs` | unverified | Missing | **R-51** |
| k | quota / usage | `Get-Usage-Limits` (`kiro.go` comments; plugin `usage.test.mjs`) | `quota.rs` with profile ARN (`lib.rs:63-69`) | Partial | Builder ID profile ARN defaults (**R-51**) |
| l | model list | Plugin lists from API | `models.rs` | Partial | Profile-scoped list refresh |
| m | multi-account | uid by login provider (`kiro/index.mjs:53-65`) | Account ids from label/provider | Partial | Stable ids (#607) (**R-24**) |
| n | error classification | Plugin `errors.test.mjs`; refresh 400 → lapsed | Generic auth on 401/403 (`upstream.rs:60-63`) | Partial | **R-24** |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | IDE cache discover | `agent_sign_in.rs` + discover | Partial | CLI DB discover |
| q | extras | Builder ID profile ARN constant (`kiro/index.mjs:50-51`) | `BUILDER_ID_PROFILE` in `creds.rs:25-26` | Partial | External-idp refresh (**R-51**) |

### Gaps (Kiro)

- Streaming Messages upstream with tools/images — **L**, **R-51**
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

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: device flow | `copilot_signin.go:31-38`, `59-86` GitHub device OAuth app (client id in `copilot_signin.go`) | `sign_in.rs`, `lib.rs:138-150` device flow | Done | — |
| a2 | sign-in: browser OAuth | N/A (device only for reference-added accounts) | N/A | N/A | — |
| a3 | sign-in: API key | N/A | N/A | N/A | — |
| a4 | sign-in: paste | N/A | N/A | N/A | — |
| a5 | sign-in: editor / CLI login reuse | `copilot_accounts.go:3-8`, `copilot_cli.go:81+` read editors’/CLI OAuth, never write | no editor config read in copilot plugin | Missing | Read Copilot CLI/editor side login (R-48) |
| a6 | sign-in: GHE host option | `copilot_ghe.go`, `CopilotHost` in sign-in | `site.rs:9-40`, `lib.rs:72-81` `host` config → `.ghe.com` URLs | Done | — |
| b | import sources | Side login from editor token metadata (`copilot_accounts.go`) | none | Missing | Side-login import (R-48) |
| c | token storage | GitHub token in `logins.json`; Copilot session token minted per request | encrypted account secret + `TokenCache<Session>` (`session.rs`) | Done | — |
| d | refresh | GitHub token long-lived; Copilot `copilot_internal/v2/token` remint (`copilot_accounts.go` session) | `session.rs` cache with margin, 401 remint (`upstream.rs:105-114`) | Done | — |
| e | upstream wire | Session `endpoints.api` (githubcopilot.com or GHE host) JSON | `upstream.rs:81-90` bearer to session base | Done | — |
| f | inbound shapes | Chat, Responses, Messages per model endpoints | `models.rs:242-267`, `upstream.rs:54-72` protocol pick + policy accept | Done | — |
| g | streaming | Supported on chat/responses | `wire::execute` stream path | Done | — |
| h | tool calls | Copilot tool schemas | standard IR tools on wire | Partial | unverified against GHE tenant |
| i | images | Vision header when images present | `upstream.rs:137-139` `Copilot-Vision-Request` | Done | — |
| j | reasoning | model-dependent | unverified | Partial | — |
| k | quota / usage | `copilot_internal/user` plan, premium requests | `quota.rs` GitHub Copilot user API | Partial | Plan label / SKU display (`copilot_entitlement.go`) (R-48) |
| l | model list | From session + internal APIs | `models.rs` list + terms acceptance | Done | — |
| m | multi-account | Editor account + reference device accounts (`copilot_accounts.go`) | multiple reference-signed accounts only | Partial | Editor “side” account pool (R-48) |
| n | error classification | `copilot_lapsed.go`, `copilot_refused.go` | 401 session remint; seat check at sign-in (`sign_in.rs`) | Partial | Entitlement/lapsed mapping parity (R-48) |
| o | daily check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in switch | Editor account is read-only reference | no agent sign-in on copilot spec | N/A | — |
| q | extras | Editor version headers, initiator, API version | `session.rs:26+`, `upstream.rs:128-140` | Done | GHE live tenant verification (R-48) |

**Gaps**

- Read VS Code / Copilot CLI side login without overwriting — **M**, **R-48**.
- Plan/SKU labels and entitlement refresh on quota cards — **S**, **R-48**.
- GHE data-residency integration test — **S**, **R-48**.

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
| k | quota / usage | `retrieveUserQuota` per model windows | `quota.rs` buckets from same RPC | Done | — |
| l | model list | `fetchAvailableModels` + tier entitlements (`google.go:1073+`) | `quota.rs` models from `retrieveUserQuota` only (`lib.rs:358-364`) | Partial | `fetchAvailableModels` + tier handling (R-49) |
| m | multi-account | Multiple google logins | multiple accounts | Done | — |
| n | error classification | RetryInfo → retry-after (`codeassist.go`) | `wire` retry_delay in feature-parity doc | Partial | Individual-account refusal persistence (`google.go:330+`) |
| o | daily check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | read gemini home only in reference docs | no `AgentSignIn` on gemini spec | Missing | Optional discover of `~/.gemini/oauth_creds.json` (R-49) |
| q | extras | Code Assist Standard/Enterprise project onboarding; model-name mapping from the reference catalog | `project.rs` loadCodeAssist/onboardUser; model-name mapping not applied (`docs/feature-parity.md` row 3.4) | Partial | Model name mapping (R-49) |

**Gaps**

- `fetchAvailableModels` and entitlement-tier fallbacks — **L**, **R-49**.
- Google account import formats — **M**, **R-49**.
- Gateway-grade tool/thought-signature edge cases — **M**, **R-49**.

---

## Google Antigravity

**Reference files read:** `google.go:82-100` (antigravity app, daily vs prod base), `antigravity_models.go`, `antigravity_pool.go`, `google_import.go`, `codeassist.go` (antigravity cloak, effort variants).

**Community:** `packages/antigravity-cloak/` (middleware for turned-away prompts).

**hofund path:** none (`docs/feature-parity.md:3.4` “Antigravity is not started”; agent adapter only under `plugins/agents/antigravity/`).

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: loopback OAuth (Antigravity client) | `google.go:82-90` separate client, `/oauth-callback`, `localhost` | missing subscription plugin | Missing | New plugin with Antigravity OAuth client (R-49) |
| b | import sources | `google_import.go` refresh token imports | none | Missing | Same import module as gemini family (R-49) |
| c | token storage | logins.json + in-memory refresh | none | Missing | R-49 |
| d | refresh | same as Google subscriptions | none | Missing | R-49 |
| e | upstream wire | `daily-cloudcode-pa.googleapis.com` + prod load base (`google.go:98-100`) | none | Missing | R-49 |
| f | inbound shapes | Same Code Assist envelope; effort variant IDs (`antigravity_models.go`) | none | Missing | R-49 |
| g | streaming | SSE envelope | none | Missing | R-49 |
| h | tools | tool declarations and thought signatures (`internal/gateway/codeassist.go`) | none | Missing | R-49 |
| i | images | inline image parts on the Code Assist wire (`internal/gateway/codeassist.go`) | none | Missing | R-49 |
| j | reasoning | thought signatures / thinking config, cloak and turned-away detection (`codeassist.go`, `packages/antigravity-cloak`) | none | Missing | R-49 |
| k | quota / usage | `retrieveUserQuota` + per-account fetchAvailableModels union | none | Missing | R-49 |
| l | model list | `antigravity_pool.go:15-49` union across accounts; effort folding (`antigravity_models.go`) | none | Missing | R-49 |
| m | multi-account | Pool catalog union | none | Missing | R-49 |
| n | error classification | 429 “turned away” prompts (`codeassist.go:51-66`) | none | Missing | R-49 |
| o | check-in / warmup | N/A | N/A | N/A | — |
| p | agent sign-in | Antigravity desktop agent separate | agent.toml only, no subscription | Missing | R-49 |
| q | extras | Ban-risk warning in UI; version manifest URL (`google.go:68`) | unverified | Missing | R-49 |

**Gaps**

- Entire Antigravity subscription plugin — **L**, **R-49**.
- Multi-account model pool + effort variant catalog — **M**, **R-49**.
- Prompt cloak / turned-away detection — **M**, **R-49**.

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

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: browser (Tencent page poll) | plugin `index.mjs:5-7` | `lib.rs:165-191` instructions to import desktop file only | Missing | Browser OAuth sign-in (R-55) |
| a2 | sign-in: API key | API-key accounts (`packages/workbuddy/index.mjs:404-418` `keyAccount`, `apikey.test.mjs`) | no API-key credential path | Missing | API-key sign-in (**R-55**) |
| a3 | desktop login reuse | read `workbuddy-desktop.info` / AI variant | `discover.rs:14-31`, `auth_store.rs` | Done | — |
| b | import sources | desktop JSON under CodeBuddyExtension data | same paths in discover | Done | — |
| c | token storage | plain JSON on disk (plugin) | encrypted hofund secret + desktop path in blob | Done | — |
| d | refresh | desktop token re-read + OAuth refresh in plugin | `refresh.rs` ensure_fresh / desktop re-read | Partial | Encrypted desktop store when protection on (R-55) |
| e | upstream wire | `{endpoint}/v2` streamed chat, gzipped body with `Content-Encoding: gzip` (`index.mjs:7-9`, `1502-1519`) | plain JSON to `{base}/v1/chat/completions` (`upstream.rs:50-51`), no gzip | Partial | `/v2` path + gzip bodies (**R-55**) |
| f | inbound shapes | OpenAI chat + WorkBuddy headers | OpenAI chat + `creds::bearer_headers` | Done | — |
| g | streaming | stream only in plugin | wire streaming | Done | — |
| h | tools | `fitTools`, `tools.test.mjs`, `aftertools.test.mjs` | unverified | unverified | Confirm in **R-55** |
| i | images | `/v2/images/*`, `images.test.mjs`, `declared-images.test.mjs` | unverified | unverified | Confirm in **R-55** |
| j | reasoning | effort lists, `reasoning.test.mjs`, `efforts.test.mjs` | unverified | unverified | Confirm in **R-55** |
| k | quota / usage | profile usage page + billing API | `quota.rs` billing without guessed totals | Partial | — |
| l | model list | `/v3/config` in plugin | `models.rs` config fetch | Done | — |
| m | multi-account | `workbuddy` vs `workbuddy-ai` provider ids | single plugin, `site::Site` CN vs International (`lib.rs:31-35`) | Partial | Separate subscription ids if needed for routing labels (R-55) |
| n | error classification | `WBRestricted` code 11140 (`workbuddy.go:267-299`) | not found in workbuddy plugin | Missing | Restricted account rest + hint (R-55) |
| o | daily check-in | `workbuddy_checkin.go` + plugin | `checkin.rs`, `with_daily_checkin` | Partial | CN vs AI check-in parity (R-55) |
| p | agent sign-in | desktop auth paths | `AgentSignIn` workbuddy + workbuddy-ai (`lib.rs:31-35`) | Done | — |
| q | extras | User-Agent must be WorkBuddy/version (`index.mjs:8-9`) | client headers in `creds.rs` | Partial | Post-restriction chat headers (R-55) |

**Gaps**

- Browser-based WorkBuddy sign-in (not file import only) — **M**, **R-55**.
- API-key accounts — **S**, **R-55**.
- `/v2` chat path and gzip request bodies — **S**, **R-55**.
- WBRestricted / 11140 account-level rest — **M**, **R-55**.
- Encrypted desktop credential store — **M**, **R-55**.

---

## Z Code (`zcode`)

**Reference files read:** `zcode.go`, `zcode_device.go`, `zcode_team.go`, `migrate_zcode.go`; plugin `packages/zcode/index.mjs`, `README.md` (check-in gifts).

**hofund path:** `plugins/subscriptions/zcode/`.

| # | capability | reference v0.1.3005 | hofund 08035c9 | status | gap to close |
|---|------------|----------------------|----------------|--------|--------------|
| a1 | sign-in: device / portal OAuth | plugin sign-in flows in `index.mjs` | `lib.rs:166-176` import/discover only | Partial | Full portal sign-in in plugin (R-57) |
| a2 | API key import | plugin supports key plans | credential import via agent store | Partial | — |
| a3 | paste / browser | plugin OAuth | unsupported in start_sign_in | Missing | R-57 |
| b | import sources | encrypted `~/.zcode/v2/credentials.json` (`zcode.go:14-18`) | `creds.rs` decrypt + `discover.rs` | Done | — |
| c | token storage | logins.json for reference-added; encrypted agent store read | SecretObject + device mid (`creds.rs:459+`) | Done | — |
| d | refresh | JWT refresh near expiry in plugin | `refresh.rs` business API refresh | Partial | unverified vs plugin JWT rules |
| e | upstream wire | Anthropic plan API bases (`zcode.go:47-48`); Start Plan accounts go to `START_BASE` (`zcode/index.mjs:33`, used at `:2371`) | `upstream.rs:66-74` Messages on plan base; JWT-only Start Plan import refused (`tests.rs:424`) | Partial | Start Plan routing (R-57) |
| f | inbound shapes | Messages only upstream | `upstream.rs:60-63` rejects non-Messages | Done | — |
| g | streaming | Anthropic SSE | wire streaming | Done | — |
| h | tool calls | Anthropic tools | standard Messages wire | Partial | unverified |
| i | images | per-model `supportsImage` (`packages/zcode/index.mjs:1690`) | unverified | unverified | Confirm in **R-57** |
| j | reasoning | per-model reasoning levels mapped to thinking budgets (`packages/zcode/index.mjs:1691`, `1753-1765`) | unverified | unverified | Confirm in **R-57** |
| k | quota / usage | monitor `/api/monitor/usage/quota/limit` personal/team | `quota.rs` monitor endpoints | Done | — |
| l | model list | portal + plan APIs in plugin | `models.rs` | Partial | Gift/check-in tied models unverified |
| m | multi-account | team seats from `setting.json` (`zcode_team.go`) | `team.rs` project keys | Done | — |
| n | error classification | ZCodeRestricted hint code 3012 (`workbuddy.go:254-260` shared pattern) | unverified in zcode plugin | Partial | R-57 |
| o | daily check-in | plugin `auth.checkin` gift plans (`index.mjs:2740+`) | no check-in in zcode plugin | Missing | Daily gift check-in, with the 30-minute recheck the reference applies to `zcode` (`plugin_checkin.go:274`) (R-57) |
| p | agent sign-in switch | agent switching into Z Code config (reference gap in subscriptions.md) | `agent_sign_in.rs` write | Partial | Agent connect for zcode (R-57) |
| q | extras | `X-Device-Mid`, `X-Device-Mid` (`api.rs:80` on the hofund side); plugin pins `APP_VERSION` 3.17.2 for User-Agent and `X-ZCode-App-Version`, and the vendor gates on it (`zcode/index.mjs:34-38`); core `zcode.go:55` still says 3.14.3 | `device.rs`; `APP_VERSION` 3.14.3 in `lib.rs:45` | Partial | Send 3.17.2 in User-Agent and `X-ZCode-App-Version` (R-57) |

**Gaps**

- Daily marketing/check-in gift flow — **M**, **R-57**.
- Native portal sign-in (not import-only) — **M**, **R-57**.
- Risk-control / restricted account handling (3012) — **S**, **R-57**.
- Agent switching parity — **S**, **R-57**.
- Start Plan routing to the plan base (`START_BASE`) — **S**, **R-57**.
- Send app version 3.17.2 in User-Agent and `X-ZCode-App-Version` — **S**, **R-57**.

---
