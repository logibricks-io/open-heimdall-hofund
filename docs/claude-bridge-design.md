# Claude subscription bridge (R-6, Claude H8b)

Status: design only. The Claude subscription plugin signs in, refreshes, reads quota and models (R-5), and still returns unsupported from `upstream()` (`plugins/subscriptions/claude/src/lib.rs:509-516`). This document is the contract for serving a model call on a Claude Pro/Max account by driving the local Claude Code binary through an MCP bridge, matching parity rows e, f, g, h, i, n, o, q and the WSL/proxy gap in `docs/coverage/subscription-parity.md`.

Wire behaviour is taken from the reference gateway bridge (`internal/gateway/claude_subscription.go` and its tests), the stdio MCP helper (`internal/claudebridge/mcp.go`), and provider sign-in (`internal/provider/claude_signin.go`, `claude_auth.go`). Hofund maps inbound Chat Completions, Responses, and Anthropic Messages in the gateway codec to `hofund_ir::Request`; the plugin owns Claude Code, bridge state, and stream decoding. Vendor names stay in `plugins/subscriptions/claude` and in this document where the wire requires them. Kernel and gateway changes here are generic (callback listener, process streaming, optional concurrency slot). Plugins still depend only on `hofund-plugin-api` and `hofund-plugin-sdk` (`docs/plugins/boundary.md`).

## 0. Decision, and where it is weak

Ship the reference’s long-lived Claude Code run per conversation owner, with MCP tool parking, idle reuse, and session shelve/resume—not a fresh `-p` process per HTTP request. That matches how subscription prompt cache is written inside Claude Code (`claude_subscription.go:120-136`, `3317-3320`) and how tool results must unblock the same process (`claude_subscription.go:9-13`; `mcp.go:93-98`).

This will be wrong if R-6 tries to put bridge maps (`runs`, `idle`, `calls`, `shelf`) in `crates/kernel/gateway` or `router`. Those structures are vendor-specific; the plugin holds them behind `UpstreamProvider::execute`. The gateway only decodes/encodes protocols and forwards IR.

This will also be wrong if the MCP callback is reachable off loopback or without a per-run secret. The reference fails closed: wrong token → 404 (`claude_subscription.go:5373-5380`; `lan.go:540-563`).

Defer interactive TUI mode (`claudeTUIOn`, `claudetui` paths in `claude_subscription.go:1046-1053`, `6204-6211`) to a follow-on slice unless a product task explicitly requires it. R-6 targets the `-p` + `stream-json` path exercised by `claudeCLIArgs` (`claude_subscription.go:3244-3263`) and most subscription tests.

## 1. Process model

### 1.1 From HTTP request to Claude Code argv

Entry on the reference side is `serveClaudeSubscription` (`claude_subscription.go:6041-6090`):

1. Parse body to an internal `Request` (Messages vs Chat/Responses branches at `6135-6148`).
2. Apply effort/thinking rules (`6152-6161`, `6072-6076`).
3. Resolve account config directory via `p.Account.Token` (`6079-6085`).
4. `resume` idle run if the turn continues a conversation (`6057-6058`; `1771-1897`).
5. Else `unshelve` / branch session, `retire` competing runs (`6060-6071`; `2117+`).
6. `start` → `startTrusted` → `startRun` (`880-915`, `918+`).

Hofund path: router selects a subscription member; daemon calls `UpstreamProvider::execute` with `hofund_ir::Request` and inbound `ProtocolId`. The Claude plugin’s `execute` performs the same ordering using an internal `Bridge` type (new module under `plugins/subscriptions/claude/src/`).

### 1.2 Binary resolution, WSL, proxy

Reference CLI selection (`claude_signin.go:85-92`):

- Prefer native `claudeExecutable()`.
- On Windows when absent, use WSL `wslrun.Find("claude")` (`85-92`).
- `ClaudeInWSL` exposes distro name without blocking first probe (`95-111`).

Per-login proxy for Bun-based Claude Code (`claude_signin.go:139-145`): `netproxy.EnvForBunWith` bridges SOCKS to HTTP when needed; WSL needs shared loopback with Windows host (`139-145`).

Hofund:

- Port WSL discovery into `plugins/subscriptions/claude/src/cli.rs` (or `wsl.rs`), invoked from `program()` (`lib.rs:221-229`) and from run/spawn paths.
- Read per-account proxy choice from daemon account metadata (same channel the reference passes through `p.Via(ctx)` at `6056`) and apply `proxied` equivalent before spawn.
- Document in tests: fake runner records env keys `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` without network.

### 1.3 Working directory and trust fallback

`startRun` chooses `work` (`claude_subscription.go:989-1038`):

- Client folder from request when local and not `passedOver` (`989-996`, `115-118`).
- Else gateway-owned work dir (`998-1007`, `clientWorkDir` / `claudeWorkDir` helpers).
- Untrusted caller folder → `tuiUntrusted` → retry in gateway folder (`903-914`).

Hofund: carry `working_directory` from IR metadata if the gateway attaches it (same as reference `clientWorkDir`). Implement `passedOver` with `untrusted` map (`96-118`, `909-914`). Plugin state dir `{state}/claude-bridge/work` replaces the reference app work dir; mode `0700`.

Cleanup: each run gets `MkdirTemp` prefix `hofund-claude-` (`945-948` analogue); `cleanup()` on failed start (`949-957`); remove tree on run exit (`subscriptionRun.tmp`). Cross-process account dir uses existing `run_lock` (`run_lock.rs:155-209`) before starting Claude Code in `{state}/accounts/<id>`.

### 1.4 Config directory and setup-token accounts

Per saved account, credentials are materialized under `{state}/accounts/<id>` for the duration of a run (`account.rs:3-6`, `78-81`). Environment scrub + injection (`inClaudeDir`, `claude_subscription.go:3266-3290`):

- Set `CLAUDE_CONFIG_DIR` to account dir.
- Strip conflicting vars (`3293-3305`).
- Setup-token: set `CLAUDE_CODE_OAUTH_TOKEN` from dir token, never argv (`3284-3288`); hofund already defines `OAUTH_TOKEN_VAR` (`cli.rs:21-22`) and `run_env` (`account.rs:44-51`).

### 1.5 Command line and MCP config

Core args from `claudeCLIArgs` (`3244-3263`):

```text
-p --output-format stream-json --input-format stream-json --include-partial-messages
--verbose --model <id> --tools <own|"">
--strict-mcp-config --mcp-config <json> --setting-sources ""
--settings <disable deep link> --dangerously-skip-permissions --no-session-persistence
```

Optional: `--effort`, `--thinking-display summarized`, `--thinking disabled` when client disables thinking (`975-981`), `--json-schema`, `--system-prompt-file`, `--append-system-prompt-file`, `--resume <session>` (`991-1115`).

MCP server entry (`971-973`): JSON `mcpServers` pointing at **hofund’s MCP helper executable** with args `[helper, callbackPath, toolsPath, listedPath]` (four-arg interactive wait file per `mcp.go:54-73`, `claudebridge/mcp_test.go`).

Environment additions from `cleanClaudeEnv` (`3293-3320`): disable auto memory/MCP servers, set `CLAUDE_CODE_PROMPT_CACHE_TTL=1h`, git instructions off, etc.

Hofund spawn uses `SpawnRequest` via `cli::request_with_extra` (`cli.rs:68-77`) plus proxy and `inClaudeDir` pairs. Kernel clears other env; plugin supplies allowlist only.

### 1.6 Long-lived run vs one-shot

| Kind | When | Reference |
|------|------|-----------|
| Long-lived `-p` run | Default turn; stays alive across turns and tool parks | Header comment `15-20`; idle map `78` |
| Resume same process | New user turn after assistant reply without tool pending | `resume` `1771-1897` |
| Tool park | `tools/call` blocked; HTTP request with tool results | `match` `4839-4924`; `mcpCall` `5368+` |
| Fresh process | No idle run; replay/resume failed; fork conflict | `start` retry `890-893`; `serveSubscription` 6213-6223 |
| Warm-up / usage | Separate short `-p` invocations | `claude_warmup.go:519-531`; `cli.rs:16-17` |

Run timeout: `runTimeout = 30 * time.Minute` (`claude_runs.go:61`); reset on activity via `endIn` (`3343-3348`, `1241-1242`, `1875`).

Idle retention: up to `idleMost = 6` runs, max wait `idleLongest = 1h` matching cache TTL (`132-136`, `2293+` shelve path).

Parked on tools: `parkLongest = 5m`, `parkedMost = 16` (`138-159`).

### 1.7 Reuse, resume, fork, rewind

- **Turn key** and `nextTurn` decide resume vs new messages (`1900+`).
- **Shelve / unshelve** session files when idle evicted (`2365-2413`).
- **Replay session** when no `--resume` but on-disk transcript can be written (`1083-1089`; `claude_replay.go:12-80`).
- **Branch / cut session** when another request answered tools first (`1054-1070`, `6224-6223`).
- **Rewind** on client cancel 499 (`22-28`; `letGo` paths in `retire` `2117+`).
- **Effort change** on idle run: non-TUI may `setEffort`; TUI aborts (`1881-1891`).

Hofund implements the same state machine in the plugin; owner string matches reference (`6043-6053`): account id + user + agents-own suffixes.

### 1.8 Concurrency and run lock

Reference global cap: `claudeRoom` (`claude_room.go:21-47`); wait `claudeRoomWait` 2m then 503 (`45-47`, `49-69`).

Hofund:

- Keep per-account `run_lock` (`run_lock.rs`) for cross-process exclusivity with refresh/warmup (already used in `account.rs:66`).
- Add plugin-local counting semaphore for concurrent Claude Code processes (config key `claude-runs-most` mirroring reference settings `claude_room.go:72-78`), default from available memory heuristic in reference `claudeMemRuns`.

### 1.9 Abort and cleanup

- Client disconnect → abort run or rewind per reference `letGo` / `abort` (`5768+`).
- Gateway shutdown → `abortAll` (`5807+`).
- Idle sweep of project/session dirs (`939-943`, `1037-1038`).

Plugin registers daemon shutdown hook via host job API to kill children and drop callback tokens (fail closed).

## 2. MCP bridge

### 2.1 What it is

Claude Code loads a **stdio MCP server** child. The reference reuses the gateway binary with hidden subcommand `claude-mcp-helper` (`claudetui/command.go:45-51`). The helper speaks JSON-RPC on stdin/stdout (`mcp.go:102-217`):

- `initialize`, `tools/list`, `tools/call` only (`144-201`).
- `tools/list_changed` notification when tool set updates (`114-128`, `194-198`).

Claude Code is configured via `--mcp-config` JSON (`971-973`) to spawn that helper.

Hofund ships the same protocol in **`hofundd` or `hofund` hidden subcommand** (implemented in `apps/cli` or `crates/kernel/daemon`, not in the plugin crate—plugins cannot use `std::process`). The plugin resolves the absolute helper path through a new `HostContext::bridge_helper()` API returning the current executable plus subcommand name constant defined in plugin (`plugins/subscriptions/claude/src/mcp.rs`).

### 2.2 Callback transport

Flow (`mcp.go:159-198`, `claude_subscription.go:959-967`):

1. Bridge writes random token and full callback URL to a mode-`0600` file (`callbackPath`).
2. Helper reads URL from file if arg is path (`mcp.go:75-82`) so secrets never appear in argv.
3. On `tools/call`, helper **POSTs JSON** `{tool_call_id,name,arguments}` to callback URL.
4. HTTP client disables proxy (`mcp.go:99-101`).
5. Bridge responds `{content,is_error,tools?}` JSON; helper returns MCP result.

Reference callback route: `POST` on the gateway listen address with a private path prefix and `{token}` suffix (`claude_subscription.go:869-871`, `gateway.go:1031`). Auth bypass for valid token only (`lan.go:528-563`).

Hofund: **`BridgeCallbackBackend`** (section 5) registers `POST /v1/hofund/internal/bridge/{token}` on **127.0.0.1 only**, or a dedicated loopback port returned as `callback_base_url()`. Constant-time token compare; unknown token → 404 with body `unknown or expired bridge run` (mirror `claude_subscription.go:5379`). No bearer token on this path; fail closed for all other routes.

### 2.3 Tool parking protocol

1. Claude Code emits tool_use in stream-json; bridge emits IR tool-call events to client.
2. MCP helper blocks in `tools/call` until callback HTTP completes (`mcp.go:93-98`).
3. Client’s **next** HTTP request carries tool results; bridge `match` finds run (`4839-4924`), delivers results into stdin (`tell`), unblocks helper.
4. Race: up to 20 × 25ms wait for call registration (`4891-4923`).
5. Optional `early` map if result arrives before registration (`5436-5440`).
6. Tool ID remap for clients that rewrite ids (`4927-4937`).

Final-only tools (`StructuredOutput`) use `finalGrace` 30s (`144-152`).

### 2.4 Timeouts and keepalive

- MCP HTTP read body limit 16 MiB (`5382`).
- Helper HTTP client: no explicit timeout in reference; bridge should use bounded read on callback handler matching plugin-host HTTP defaults.
- Search tool path uses 55s context timeout (`5414-5415`).
- Run timer `runTimeout` (section 1.6).
- Park patience `parkLongest` releases run to fresh whole-history start (`138-141`).

No separate TCP keepalive; long-lived stdio + blocked MCP calls hold the turn open.

### 2.5 Security checklist

| Rule | Reference | Hofund |
|------|-----------|--------|
| Callback bind loopback | `callbackBaseURL` forces `127.0.0.1` when listen is wildcard (`857-866`) | Bridge listener `127.0.0.1` only |
| Token unguessable | `randomToken()` (`963`) | `host.random()` 128-bit hex |
| Token not in argv | file path arg (`959-967`, `mcp.go:75-82`) | same |
| MCP POST unauthenticated but token | `ownCallback` (`lan.go:540-563`) | bridge route only |
| Proxy disabled on callback | `mcp.go:99-101` | reqwest client `no_proxy` in host |
| Tools file mode 0600 | `952-957` | same |
| Logs | stderr cap 1 MiB (`3337-3339`) | redact tokens in plugin logs |

## 3. Request mapping

### 3.1 Inbound protocols

Gateway codec already produces `hofund_ir::Request`. The plugin receives:

- **Anthropic Messages** — direct mapping of system, messages, tools, thinking, effort (`serveClaudeSubscription` Messages branch `6135-6136`).
- **Chat Completions / Responses** — reference `parse(from, body)` path (`6146-6147`); hofund gateway must translate before `execute` (no change in R-6 plugin if already routed through codec).

Fields to honor on the internal request (reference `Request` struct usage in `6041-6211`):

| Field | Source | Reference lines |
|-------|--------|-----------------|
| Model id | Provider `OwnModelID` | `6086-6088` |
| Effort | body `output_config` / auto classifier → low | `6072-6076`, `6152-6161` |
| Think off | `--thinking disabled` | `975-981` |
| Tools + tool_choice | `bridgeTools`, `allowedTools` | `951`, `1873`, `5426-5431` |
| Images | URL fetch in TUI; `-p` uses blocks | `6187-6192`; `claude_images.go` |
| JSON schema / structured output | `--json-schema`, answer tool in TUI | `982-984`, `6194-6202` |
| Web search | inject search tool | `6204-6210` |
| Safeguards beta | header | `6178-6185` |
| Client cache marks | Anthropic cache_control | `6164-6169`, `6135-6142` |

### 3.2 Prompt construction

- Full history: `renderClaudePrompt` when no resume (`1093-1097`).
- Partial turn: `renderClaudeTurn` on `since` slice (`1120-1121`).
- Lean system / append files when settings demand (`1009-1115`).
- Text-form past tool calls + `textCallsAsk` (`claude_text_calls.go:21-24`; stream handler `3806-3818`).

Images in replay/resume: `claude_images.go` (cited in parity row i).

### 3.3 Outbound stream-json → IR

Reader loop on Claude Code stdout (`claude_subscription.go:3791+`):

- Parse JSON lines (stream-json).
- Map envelope types to `StreamEvent`: text deltas, thinking, tool starts/args, usage, stop, error (`4046+` `message_delta`, assistant blocks, tool_use).
- `rate_limit_event` → update quota windows via existing R-5 hooks (`3797-3804`; `provider.NoteClaudeLimits` analogue in plugin calling `host` quota report API if present).
- `system/init` → capture `sessionID` (`3791-3795`).
- `result` errors → terminal error (`3835-3878`).

Usage numbers: extract from result/message_stop events per reference tests (`claude_tui_usage_test.go` patterns); map to `hofund_ir` usage fields on `MessageEnd` / completion.

### 3.4 Tool results inbound

HTTP second leg: `match` extracts tool result parts (`4844-4871`), remaps ids (`4927+`), writes stream-json user message to stdin (`1863-1896` pattern).

Hofund `execute` must accept continuation requests: same conversation key in IR (gateway affinity / session headers already used elsewhere). If hofund IR lacks explicit conversation id, derive **owner key** from daemon account id + optional agent-own fields matching reference `owner` (`6043-6053`).

## 4. Error classification

Map bridge output to `PluginError` kinds (`crates/kernel/plugin-api/src/error.rs:16+`) and router rests (`crates/kernel/router/src/cooldown.rs`). Do **not** add vendor strings to router crates.

### 4.1 Sign-in required (no rest)

Reference `ClaudeSignInRequired` (`claude_auth.go:25-41`) and `NoteClaudeSignInFailure` (`131-149`) on messages containing:

- `OAuth session expired and could not be refreshed`
- `OAuth token revoked` / `OAuth access token has been revoked`
- Reference lapse strings

Emit **`PluginError::auth`** only for subscription lifecycle (`refresh`, sign-in). For upstream run failures, use **`PluginError::status_error(401, msg)`** so failover works (same pattern as `docs/cursor-upstream-design.md` section 6).

On auth-shaped stream errors, plugin calls **`host.accounts()`** to mark account lapsed (mirror `refuseClaudeLogin`, `claude_auth.go:121-128`) with hofund’s `LAPSED` flag (`creds.rs`).

### 4.2 Refusal / policy

Model refusals and `model_refusal` stream paths (`3844-3850`, `3840-3841`). Map to **`status_error` with 403** when API status present; do not rest account permanently unless reference `NoteClaudeSignInFailure` applies.

Subscription quota refusal text in stream → **`PluginError::quota_status(429, ...)`** when wording matches quota classifier (`crates/kernel/plugin-sdk/src/transport.rs`).

### 4.3 Rate limit

Rate limit events and HTTP 429-like result text → **`status_error(429, ...)`** or `quota_status` when window exhausted (reference distinguishes via `claude_limits.go` headers/events).

### 4.4 Too long

Prompt too large / context errors → **`status_error(400, ...)`** with substring matching `too_long` patterns in `crates/kernel/ir/src/error.rs` (same as cursor design §6).

### 4.5 Crashed / cut run

- Process exit before complete turn → **`PluginError::transport`** or **`status_error(502, ...)`** with message `Claude Code ended` analogue (`877-878` comment).
- `claudeFull` room exhaustion → **`status_error(503, ...)`** containing `processes are running` phrasing (`claude_room.go:56-69`).
- Run lost while tools pending → message from `whyNoRun` / `lostCalls` (`6030-6037`).
- Timeout `runTimeout` → **`PluginError::timeout`** or 502 mapping.

### 4.6 Invalid grant on refresh

Already **`PluginError::auth`** in plugin refresh (`account.rs:85-88`); do not map to rested cooldown (`claude_auth.go:7-11`).

## 5. Plugin versus kernel

### 5.1 Division

| Component | Owner |
|-----------|--------|
| Bridge state machine, stream-json codec, prompt render, tool parking | `plugins/subscriptions/claude/src/bridge/` |
| MCP helper stdio loop | `apps/cli` or `crates/kernel/daemon` subcommand (generic name `bridge-mcp-stdio`) |
| Protocol translate Chat/Responses/Messages | `crates/kernel/gateway` + `codec` (existing) |
| Account dir, lease, run_lock, setup-token | existing plugin modules |
| Callback HTTP route | `plugin-host` |
| Router rests / failover | `router` (generic kinds only) |

### 5.2 Minimal plugin-api extensions

Plugins cannot bind sockets (`docs/plugins/boundary.md`). Add to `hofund-plugin-api` / implement in `hofund-plugin-host`:

1. **`BridgeCallbackHandle`**
   - `register(token, Registration)` → drop guard unregisters.
   - `base_url()` → `http://127.0.0.1:<port>` for writing callback file.
   - Handler receives POST body bytes; returns `(status, json_bytes)`.
   - Document: loopback only; constant-time token lookup.

2. **`BridgeHelperProgram`**
   - `fn bridge_mcp_helper(&self) -> SpawnRequest` — absolute path to current binary + static subcommand argv prefix (no vendor in API name).

3. **`LongLivedChild`** (extend `PipedChild` or new type)
   - `write_stdin(Bytes)`, `stdout_stream() -> BodyStream`, `kill()`, `wait() -> ExitStatus`.
   - Required for one Claude Code across turns; today sign-in uses piped spawn but does not expose a session handle to upstream.

4. **`ConcurrencySlot`** (optional if room logic stays plugin-local)
   - `acquire(label, timeout)` → `SlotGuard`; global across plugin instances in one daemon.

Kernel implementation files (expected): `crates/kernel/plugin-host/src/bridge_callback.rs`, extend `piped.rs`, wire `HostContext::bridge_callback()` in `host.rs`. No vendor names in those files.

Gateway change: allow unauthenticated POST to bridge path only when `plugin-host` validates token (mirror `ownCallback`). Generic path segment **`/v1/hofund/internal/bridge/`** — no vendor in path.

### 5.3 Dependencies

Any new workspace crate edge must update `docs/architecture.md` and `xtask/tests/workspace_invariants.rs`. `cargo deny` with `multiple-versions = deny` — prefer existing `reqwest`/`hyper` in plugin-host only.

## 6. Testing without the real binary

No network. Enforced by:

- **`FakeClaude` script** (shell or Rust) in `hofund-kit-subscription`: reads stdin stream-json lines, writes scripted stdout lines from fixture; asserts env (`CLAUDE_CONFIG_DIR`, no token in argv).
- **Injected runner** trait in plugin `#[cfg(test)]` replacing `program()` and spawn (pattern from existing `cli.rs` test hooks `test_configured_dir`).
- **MCP helper unit tests** porting `claudebridge/mcp_test.go` and `claude_callback_addr_test.go` against hyper/local connector in **plugin-host** tests (not plugin crate).
- **Golden stream fixtures** under `plugins/subscriptions/claude/fixtures/stream/` for `message_delta`, tool_use, `rate_limit_event`, `result` error.
- **Bridge integration tests** in plugin using fake runner + in-memory callback channel (no TCP) for parking: helper thread POSTs to channel simulating host handler.

Mutation tests (must fail on one-char regressions):

- Callback rejects wrong token (`claude_callback_addr_test.go:134`).
- `tools/call` blocks until result POST (`claude_subscription_park_test.go` header comment).
- Resume sends only `since` messages (`1771-1847` logs).
- `match` retry window (`4891-4923`).
- WSL env rewriting when enabled (mock).
- Proxy env present when account has proxy metadata.

Update `plugins/subscriptions/claude/src/conformance.rs` to expect `upstream()` supported after R-6 lands.

## 7. Work breakdown

Slices are ordered; later slices depend on earlier ones. **Safe before further design review:** R-6a, R-6b (API + fake tests only).

### R-6a — Host API and MCP helper (kernel/cli)

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `BridgeCallbackBackend` + loopback server | `plugin-api`, `plugin-host`, gateway auth bypass hook | 400 LOC | Callback reachable off-loopback |
| `bridge-mcp-stdio` subcommand | `apps/cli` or daemon, port `mcp.go` | 250 LOC | Drift from Claude Code MCP expectations |
| Host tests | `plugin-host/tests/bridge_*.rs` | 200 LOC | |

Deferred: handover forward (`lan.go:548-551`).

### R-6b — Plugin scaffold + fake runner

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `Bridge` struct stub, `upstream()` returns provider | `plugins/subscriptions/claude/src/{upstream,bridge}.rs`, `lib.rs` | 300 LOC | |
| Fake Claude + golden parse tests | `fixtures/`, `bridge/decode.rs` tests | 400 LOC | |

Safe pre-review.

### R-6c — Process spawn + stream decode (text only)

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `startRun` port (no resume) | `bridge/run.rs` | 800 LOC | Windows argv length → system prompt file |
| stream-json → IR text | `bridge/emit.rs` | 500 LOC | Partial message handling |
| WSL + proxy env | `cli.rs`, `wsl.rs` | 200 LOC | Wrong distro |

### R-6d — Tool parking + continuation

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `match`, mcp callback wiring | `bridge/park.rs` | 600 LOC | Client id remap bugs |
| IR tool events round-trip | `upstream.rs` | 400 LOC | Deadlock if helper never unblocked |

### R-6e — Resume, idle, shelve, room

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `resume`, `idle`, `shelve`, `retire` | `bridge/session.rs` | 900 LOC | Session file incompatibility across Claude versions |
| `claudeRoom` slot | `bridge/room.rs` + config | 200 LOC | 503 under load |

Requires review of session-on-disk format against reference `claude_sessions` helpers.

### R-6f — Images, effort, thinking, limits

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| Images | `bridge/images.rs` | 300 LOC | Large stdin |
| Effort/thinking/schema | `bridge/flags.rs` | 250 LOC | |
| `rate_limit_event` → quota | tie to R-5 `usage.rs` | 150 LOC | |

### R-6g — Error mapping + parity docs

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `failure.rs` classification | 250 LOC | Wrong rest → account stuck |
| `docs/coverage/subscription-parity.md`, `docs/magpie-comparison.md` row | docs | | |

### Explicitly deferred (out of scope)

- Sessions recording, Library agent-config writes (roadmap).
- MCP router command execution (R-38).
- Interactive TUI mode (`claudetui`) unless product asks.
- Gateway handover between daemon instances.
- Web search tool injection (reference `6204-6210`) — optional slice after R-6f.

## 8. Document summary (English)

This design specifies how hofund serves Claude Pro/Max subscription traffic by spawning Claude Code with `-p` and `stream-json`, attaching a stdio MCP helper that POSTs tool calls to a loopback-only callback, and keeping long-lived runs with idle reuse, session shelve/resume, and tool-result parking matching the reference bridge. The Claude plugin owns bridge state and stream mapping; the kernel gains generic long-lived piped processes and a loopback callback listener; the gateway continues to translate inbound API shapes to IR. Errors map to existing `PluginError` kinds and router rest rules, with sign-in failures persisted on the account but not rested blindly. Testing relies on fake Claude scripts, golden stream fixtures, and host-level MCP tests without network access. Work is split into R-6a through R-6g, with R-6a–b safe to implement before a deeper session/resume review (R-6e).

## 9. Open questions

1. **Conversation key in IR** — Does the gateway already expose a stable conversation id for subscription continuations, or must the plugin hash messages like reference `turnKey`?
2. **Helper placement** — Single `hofundd` subcommand vs tiny `hofund-bridge-mcp` binary for faster spawn?
3. **TUI mode** — Is interactive Claude Code required for any shipped agent, or can R-6 ship `-p` only first?
4. **Room limit default** — Port memory-based `claudeMemRuns` or start with a conservative fixed cap?
5. **Callback path on gateway vs sidecar** — Sidecar avoids mixing auth bypass into main mux; which matches ops expectations for LAN/Tailscale gateway binds?
