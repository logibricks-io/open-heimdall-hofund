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

Per-login proxy for Bun-based Claude Code (`claude_signin.go:139-145`): `netproxy.EnvForBunWith` bridges SOCKS to HTTP when needed; WSL needs shared loopback with Windows host (`139-145`). Bridge runs apply proxy through `proxied` → `EnvForBunWith`, not by copying caller `HTTP_PROXY` verbatim (`claude_subscription.go:799-804`).

Hofund:

- Port WSL discovery into `plugins/subscriptions/claude/src/cli.rs` (or `wsl.rs`), invoked from `program()` (`lib.rs:221-229`) and from run/spawn paths.
- When Claude Code runs inside WSL, rewrite host paths the reference passes through `binary.file` and the MCP helper executable path through `wsl.Linux` (`wslrun.go:414-424`, `530-537`; helper `command` in `--mcp-config` uses the same rewrite as the gateway executable).
- Read per-account proxy choice from daemon account metadata (same channel the reference passes through `p.Via(ctx)` at `6056`) and apply the Bun proxy helper before spawn.
- Document in tests: fake runner records `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` as set by the proxy helper, without network.

### 1.3 Working directory and trust fallback

`startRun` chooses `work` (`claude_subscription.go:989-1038`):

- Client folder from request when local and not `passedOver` (`989-996`, `115-118`).
- Else gateway-owned work dir (`998-1007`, `clientWorkDir` / `claudeWorkDir` helpers).
- Untrusted caller folder → `tuiUntrusted` → retry in gateway folder (`903-914`).

Hofund: carry `working_directory` from IR metadata if the gateway attaches it (same as reference `clientWorkDir`). Implement `passedOver` with `untrusted` map (`96-118`, `909-914`). Plugin state dir `{state}/claude-bridge/work` replaces the reference app work dir; mode `0700`.

Cleanup: each run gets `MkdirTemp` prefix `hofund-claude-` (`945-948` analogue); `cleanup()` on failed start (`949-957`); remove tree on run exit (`subscriptionRun.tmp`). Cross-process credential safety uses **`run_lock`** (section 1.8), not a long-lived `accounts().lease` (`account.rs:17-18`, `LEASE_WAIT` 30s).

### 1.4 Config directory and setup-token accounts

Per saved account, credentials are materialized under `{state}/accounts/<id>` for the duration of a run (`account.rs:3-6`, `78-81`). Environment scrub + injection follows the reference order (`claude_subscription.go:1152-1196`):

1. `cleanClaudeEnv` strips conflicting vars, including any incoming `CLAUDE_CODE_OAUTH_TOKEN` (`3293-3305`).
2. `proxied` may append Bun proxy variables (`799-804`).
3. `binary.env(binary.asRoot(env), configDir)` calls `inClaudeDir` **after** the scrub, so setup-token accounts get `CLAUDE_CONFIG_DIR` and a fresh `CLAUDE_CODE_OAUTH_TOKEN` from the account dir (`3266-3288`), never argv.

Hofund already defines `OAUTH_TOKEN_VAR` (`cli.rs:21-22`) and `run_env` (`account.rs:44-51`); bridge spawn must mirror the same ordering in `bridge/run.rs`. **Credential files:** refresh, usage, and warmup materialize `{dir}/credentials` for a short subprocess, then remove it (`account.rs:80-83`, `:139-147`). While any **`RunMark`** exists beside the account dir, those paths must **`run_lock::wait`** before writing or deleting shared credential files and must **refuse** if marks remain after the wait budget (`run_lock.rs:185-209`). The bridge holds **`mark` only** (never `wait`) for the life of the Claude Code process (`run_lock.rs:155-177`).

### 1.5 Command line and MCP config

Core args from `claudeCLIArgs` (`3244-3263`):

```text
-p --output-format stream-json --input-format stream-json --include-partial-messages
--verbose --model <id> --tools <own|"">
--strict-mcp-config --mcp-config <json> --setting-sources ""
--settings <disable deep link> --dangerously-skip-permissions --no-session-persistence
```

Optional: `--effort`, `--thinking-display summarized`, `--thinking disabled` when client disables thinking (`975-981`), `--json-schema`, `--system-prompt-file`, `--append-system-prompt-file`, `--resume <session>` (`991-1115`).

MCP server entry (`971-973`): JSON `mcpServers` with a **hofund-local server key** (not copied from the reference wire name). `command` is the helper’s **absolute path** (through WSL Linux rewrite when Claude runs in WSL). `args` are **`[bridge-mcp-stdio, callbackPath, toolsPath, listedPath]`** — four strings after the program, matching `RunMCP` (`mcp.go:64-73`). The first arg is the hidden subcommand name constant in hofund, not the helper binary path (`mcp.go` documents the reference subcommand only as a line citation; reimplement the protocol, do not copy `serverInfo.name`, env var prefixes, or comments from that file).

The callback URL is written mode `0600` into `callbackPath`; the helper reads the URL from that file when the arg is not `http(s)://` (`959-967`, `mcp.go:75-82`).

Root and WSL-root runs: when `claudeCLI.root()` is true, `asRoot` sets `IS_SANDBOX=1` unless already set or bubblewrap is configured (`744-769`), so `--dangerously-skip-permissions` is accepted.

WSL env crossing uses the reference **`Tool.Env`** rules (`wslrun.go:437-477`, `claude_subscription.go:773-796`):

- **`WSLENV`** lists `claudeEnvVars` entries present in env, account-dir vars (`CLAUDE_CONFIG_DIR/p`), and **`CLAUDE_CODE_OAUTH_TOKEN`** when set after `inClaudeDir`.
- When the distro user is root, also list **`IS_SANDBOX`** (`792-795`) so `asRoot`'s value reaches the distro.
- **`Tool.Env` always forwards proxy variables** (`434-470`, `462-468`) and rewrites loopback hosts in proxy URLs to the Windows NAT address via **`Tool.proxy`** (`479-497`).

Prompt and system files: paths passed to Claude Code use `binary.file(path)` (`807-813`), not the client working directory when Claude runs in WSL (`990-995`). Lean system prompts use `--system-prompt-file` under the run temp dir (`1009-1017`).

Environment additions from `cleanClaudeEnv` (`3293-3320`): disable auto memory/MCP servers, set `CLAUDE_CODE_PROMPT_CACHE_TTL=1h`, git instructions off, etc.

Bridge spawn uses `LongLivedChild` / `SpawnRequest` via `cli::request_with_extra` (`cli.rs:68-77`) with **only** explicit env pairs from the plugin (proxy helper, `asRoot`, `inClaudeDir`, WSL `WSLENV`). **`LongLivedChild` must not inherit the host proxy allowlist** from ordinary piped spawn (`piped.rs:61-63`, `33-48`), which would contradict section 1.2.

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

**One-off turns do not idle:** when a run ends with `stop`, **no tools**, and **no assistant reply yet** (title/classifier one-shots), the reference **`abort`s** instead of parking in `idle` (`2200-2207`). Otherwise idle slots fill with processes that never get a continuation turn. Fake-runner test in **R-6c/R-6e** must assert such a run is killed, not keyed in `idle`.

Parked on tools: `parkLongest = 5m`, `parkedMost = 16` (`138-159`).

### 1.7 Reuse, resume, fork, rewind

- **Turn key** and `nextTurn` decide resume vs new messages (`1900+`).
- **Shelve / unshelve** session files when idle evicted (`2365-2413`).
- **Replay session** when no `--resume` but on-disk transcript can be written (`1083-1089`; `claude_replay.go:12-80`).
- **Branch / cut session** when another request answered tools first (`1054-1070`, `6213-6223`).
- **Rewind** on client cancel 499 (`22-28`; `letGo` paths in `retire` `2117+`).
- **Effort change** on idle run: non-TUI may `setEffort`; TUI aborts (`1881-1891`).

Hofund implements the same state machine in the plugin. **Idle/resume map key** is the reference `turnKey` SHA-256 (`3140-3146`, `1909-1933`, stored on the run at `2281`), not the owner string alone (`6043-6053` is only the owner prefix inside the hash). The hash covers owner, model, whether effort was requested (not the level), think-off, tool choice, system (billing header stripped), tools JSON, web search, JSON schema, **`cacheIdentity`** (`claude_cache.go:236-247`: client-cache marks and lean-prompt mode), and message bodies via `hashMessages` (tool ids excluded, `3205-3210`). A **cache-mode or lean-prompt change must not resume** an existing run (`6224-6229`). After a matching idle run is found, apply effort with `setEffort` before the turn (`1881-1891`).

### 1.8 Concurrency and run lock

Reference global cap: `claudeRoom` (`claude_room.go:21-47`); wait `claudeRoomWait` 2m then 503 (`45-47`, `49-69`).

Hofund:

- **Bridge runs:** `run_lock::mark` at start; keep the **`RunMark`** until the Claude Code process exits; **never** call `run_lock::wait` from the bridge (multiple marks may coexist, `run_lock.rs:155-177`; `wait` serializes callers, `185-209`).
- **Refresh, usage, warmup:** call **`run_lock::wait`** before materializing or deleting credential files; fail if marks remain after the wait limit (`agent_sign_in.rs:19` pattern for sign-in; extend to `account.rs:66+` paths). Do **not** hold `accounts().lease` for an entire bridge conversation (`LEASE_WAIT` 30s, `account.rs:17-18`).
- Plugin-local counting semaphore for concurrent Claude Code processes (config key `claude-runs-most` mirroring reference settings `claude_room.go:72-78`), default from available memory heuristic in reference `claudeMemRuns`.

### 1.9 Abort and cleanup

- Client disconnect → abort run or rewind per reference `letGo` / `abort` (`5768+`).
- Gateway shutdown → `abortAll` (`5807+`).
- Idle sweep of project/session dirs (`939-943`, `1037-1038`).

**Shutdown:** implement **`UpstreamProvider::shutdown`** on the Claude bridge provider to **`kill`** all `LongLivedChild` handles and **invalidate** callback registrations (fail closed). The host scheduler **`shutdown`** only clears periodic jobs and aborts one-shot futures (`crates/kernel/plugin-host/src/lib.rs:1013-1023`); it does not reap bridge children. **R-6a** extends the daemon exit path to call **`UpstreamProvider::shutdown`** on every constructed upstream instance before the process exits (not done today; default `shutdown` is a no-op, `upstream.rs:499-507`).

## 2. MCP bridge

### 2.1 What it is

Claude Code loads a **stdio MCP server** child. The reference reuses its gateway binary with a hidden MCP subcommand (`claudetui/command.go:45-51`; wire name omitted here). The helper speaks JSON-RPC on stdin/stdout (`mcp.go:102-217`):

- `initialize`, `tools/list`, `tools/call` only (`144-201`).
- `tools/list_changed` notification when tool set updates (`114-128`, `194-198`).

Claude Code is configured via `--mcp-config` JSON (`971-973`) to spawn that helper.

Hofund ships the same protocol in **`hofundd` or `hofund` hidden subcommand** (implemented in `apps/cli` or `crates/kernel/daemon`, not in the plugin crate—plugins cannot use `std::process`). The plugin resolves the absolute helper path through a new `HostContext::bridge_helper()` API returning the current executable plus subcommand name constant defined in plugin (`plugins/subscriptions/claude/src/mcp.rs`).

### 2.2 Callback transport

Flow (`mcp.go:159-198`, `claude_subscription.go:959-967`):

1. Bridge writes random token and full callback URL to a mode-`0600` file (`callbackPath`).
2. Helper reads URL from file if arg is path (`mcp.go:75-82`) so secrets never appear in argv.
3. On `tools/call`, helper **POSTs JSON** `{tool_call_id,name,arguments}` to callback URL. **`tool_call_id`** is `_meta["claudecode/toolUseId"]` when that string is non-empty, else a helper-generated id (`mcp.go:169-174`). This logic stays in the **MCP helper subcommand** (cli/daemon), not in router or gateway.
4. HTTP client disables proxy (`mcp.go:99-101`).
5. Bridge responds `{content,is_error,tools?}` JSON; helper returns MCP result.

Reference callback route: `POST` on the gateway listen address with a private path prefix and `{token}` suffix (`claude_subscription.go:869-871`, `5373-5380`). The reference may admit that POST without a gateway API key when the token matches (`lan.go:528-563`); that is wider than this design.

Hofund: **`BridgeCallbackBackend`** (section 5) runs a **separate HTTP listener bound only to `127.0.0.1`**, independent of the public gateway mux — no gateway auth bypass, no shared wildcard listen socket. `base_url()` returns `http://127.0.0.1:<port>` for the mode-`0600` callback file (`857-866` only rewrites the URL host when the main listener is wildcard; hofund does not replicate that pattern on the public gateway).

Handler rules:

- Unknown, spent, or missing token → **404** with fixed body `unknown or expired bridge run` (mirror `claude_subscription.go:5379`); do not echo the token or log the request target path.
- Valid token → constant-time compare against **every live registration** in the set (`lan.go:557-561`), not a single map lookup that short-circuits on length.
- Tokens: 24 random bytes as hex via `ring` `SystemRandom` in the plugin (same pattern as `crates/kernel/plugin-sdk/src/oauth/pkce.rs:136-137`). RNG failure fails the run closed; do not fall back to timestamps (`claude_subscription.go:697-701`).

This sits beside [`LoopbackHandle`](crates/kernel/plugin-api/src/host.rs:1203+) (one-shot browser OAuth redirect), not inside it: bridge callbacks are many concurrent POSTs with path suffix tokens.

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

**Client keepalive during quiet upstream work:** the gateway’s `: keepalive` SSE comments apply only while `dispatch` has not returned (`crates/kernel/gateway/src/lib.rs:2120-2136`). After relay starts, match **`keepBridgeHeard`** (`claude_subscription.go:6489-6536`, `gateway.go:8471`, `8482`):

- Start counting from **relay start** (`began` in `6489+`).
- After **`bridgeHeardAfter` (1 minute)** since relay start **and** at least **`keepaliveEvery` (15 s)** since the last upstream event, emit **`StreamEvent::Ping`** every **15 s**.
- Stop when quiet reaches **`translatedQuietLongest` (30 minutes)**.
- **Never** emit `Ping` before **`StreamEvent::MessageStart`**: if the reply has not started, emit an **empty `MessageStart` first** (`6524-6532`), because pings before `message_start` break Anthropic SDK clients.

### 2.5 Security checklist

| Rule | Reference | Hofund |
|------|-----------|--------|
| Callback bind loopback | Dedicated listener; URL host may be `127.0.0.1` when main listen is wildcard (`857-866`) | Separate listener on `127.0.0.1` only |
| Token unguessable | `randomToken()` (`697-702`) | `ring` `SystemRandom`, 24 bytes hex; fail closed on RNG error |
| Token not in argv | file path arg (`959-967`, `mcp.go:75-82`) | same |
| MCP POST unauthenticated but token | `ownCallback` on shared gateway listen (`lan.go:540-563`) | Token on dedicated loopback listener only |
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
- `rate_limit_event` → update quota windows only via existing R-5 hooks; **do not** end the turn or emit a terminal error (`3797-3804`; status `allowed` appears). Map to `host` quota report APIs, not `PluginError`.
- `system/init` → capture `sessionID` (`3791-3795`).
- `result` errors → terminal error (`3835-3878`); only **error** `result` envelopes with 429 or quota-shaped text become `status_error(429)` / `quota_status` (section 4).

Usage numbers: extract from result/message_stop events per reference tests (`claude_tui_usage_test.go` patterns); map to `hofund_ir` usage fields on `MessageEnd` / completion.

### 3.4 Tool results inbound

HTTP second leg: `match` extracts tool result parts (`4844-4871`), remaps ids (`4927+`), writes stream-json user message to stdin (`1863-1896` pattern).

Hofund `execute` must accept continuation requests keyed by the **`turnKey` hash** (section 1.7), not owner alone. The gateway may still attach affinity metadata; `hofund_ir::Request` has no session id field (`crates/kernel/ir/src/request.rs:16-39`).

## 4. Error classification

Map bridge output to `PluginError` kinds (`crates/kernel/plugin-api/src/error.rs:16+`) and router rests (`crates/kernel/router/src/cooldown.rs:1463-1488`, `decision.rs:171-178`). Do **not** add vendor strings to router crates.

Default `PluginError::status_error(401|403|503, …)` **does** cool and failover like other status errors (`cooldown.rs:1477-1478`). Cases below use **`.without_cooldown()`** and/or router skip rules so behaviour matches the reference gateway (`gateway.go:4226-4276`, `routing.go:658-666`).

### 4.1 Sign-in required (no cooldown)

Reference `ClaudeSignInRequired` (`claude_auth.go:25-41`) and `NoteClaudeSignInFailure` (`131-149`) on messages containing:

- `OAuth session expired and could not be refreshed`
- `OAuth token revoked` / `OAuth access token has been revoked`
- Reference lapse strings

Emit **`PluginError::auth`** only for subscription lifecycle (`refresh`, sign-in). For upstream **run** failures where `ClaudeSignInRequired` matches, map status with **`bridgeFailed(name, msg, APIErrorStatus)`** (`6408-6419`): **keep `>= 400` statuses**, else **502** — do **not** force **401**. Attach **`.without_cooldown()`** so the member is not rested (`claude_auth.go:7-12`; `failureOf` treats sign-in-shaped `>= 400` as auth skip without rest, `routing.go:658-666`). Default failover still includes 401 and 5xx (`crates/kernel/router/src/lib.rs:95-98`).

On auth-shaped stream errors, plugin calls **`host.accounts()`** to mark the account lapsed (mirror `refuseClaudeLogin`, `claude_auth.go:121-128`) with hofund’s `LAPSED` flag (`creds.rs`). Pin the refusal to **`loginVersion` captured at run start** (`1236-1238`, `NoteClaudeSignInFailure` `143-144`): a credential rotated after refresh or re-sign-in must not inherit the old refusal.

### 4.2 Refusal / policy

Model refusals and `model_refusal` stream paths (`3844-3850`, `3840-3841`). Preserve **`bridgeFailed` status rules** (`6408-6411`): when the stream carries `APIErrorStatus >= 400`, return that status and message — do **not** normalize all API failures to 403. Status-less refusals follow the reference `refusal()` paths.

**Third-party extra-usage text** (`thirdPartyWords`, `6430-6441`): keep **400**, **`.without_cooldown()`**, and **do not failover to another Claude subscription member** on the same request (reference does not ask the next Claude account for that shape).

Subscription quota refusal text in an error **`result`** → **`PluginError::quota_status(429, ...)`** when wording matches the quota classifier (`crates/kernel/plugin-sdk/src/transport.rs`).

### 4.3 Rate limit

**`rate_limit_event` envelopes are not errors** (section 3.3). Only terminal **`result`** failures with HTTP 429 or quota-shaped text map to **`status_error(429, ...)`** or `quota_status` when the window is exhausted (reference distinguishes via `claude_limits.go` headers/events on error results).

### 4.4 Too long

Prompt too large / context errors → **`status_error(400, ...)`** with substring matching `too_long` patterns in `crates/kernel/ir/src/error.rs` (same as cursor design §6). Status-less **`Prompt is too long`** from Claude Code maps to **400** via `bridgeFailed` (`6400-6417`), not 502.

### 4.5 Crashed / cut run

- Process exit before complete turn → **`PluginError::transport`** or **`status_error(502, ...)`** with message `Claude Code ended` analogue (`877-878` comment).
- **`claudeFull` room exhaustion** → **`status_error(503, msg).without_cooldown()`** where `msg` (plugin-only English) contains the reference room-full sentence (`claude_room.go:56-64`). Set a **vendor-neutral `PluginError` flag** (for example **`skip_failover_same_plugin`**, name finalized in R-6g) on that error; **`crates/kernel/router` must not embed the English sentence or vendor words** (`xtask/tests/workspace_invariants.rs:299-304`, `:1369-1383`). Router skips **only later route members with the same subscription plugin id**; other plugins may still failover (`gateway.go:4252-4276`). Plain **`status_error(503)`** without the flag still cools and fails over normally.
- Run lost while tools pending → message from `whyNoRun` / `lostCalls` (`6030-6037`).
- Timeout `runTimeout` → **`PluginError::timeout`** or 502 mapping.

### 4.6 Invalid grant on refresh

Already **`PluginError::auth`** in plugin refresh (`account.rs:85-88`); do not map to rested cooldown (`claude_auth.go:7-11`).

### 4.7 Router follow-ups (R-6g)

- **`skip_failover_same_plugin`** (or equivalent) on `PluginError` in **`hofund-plugin-api`**, set only by the Claude plugin on room-full 503. Router failover reads the flag and drops remaining candidates with the **same plugin id**; no message substring matching in `crates/kernel/router`.
- Third-party extra-usage 400: **`.without_cooldown()`** only; default failover does not try another member for bare 400 (`decision.rs:171-178`, `lib.rs:95-98`) — no extra router rule required (optional follow-up note in review).

## 5. Plugin versus kernel

### 5.1 Division

| Component | Owner |
|-----------|--------|
| Bridge state machine, stream-json codec, prompt render, tool parking | `plugins/subscriptions/claude/src/bridge/` |
| MCP helper stdio loop | `apps/cli` or `crates/kernel/daemon` subcommand (generic name `bridge-mcp-stdio`) |
| Protocol translate Chat/Responses/Messages | `crates/kernel/gateway` + `codec` (existing) |
| Account dir, lease, run_lock, setup-token | existing plugin modules |
| Callback HTTP route | `plugin-host` loopback listener (not gateway mux) |
| Router rests / failover | `router` (generic kinds and flags only) |

### 5.2 Minimal plugin-api extensions

**Owner slice:** **R-6a** lands these APIs in `hofund-plugin-api` and `hofund-plugin-host`. The Claude plugin consumes them from **R-6c** onward; bridge state owns child handles in the plugin crate.

Plugins cannot bind sockets (`docs/plugins/boundary.md`). Add to `hofund-plugin-api` / implement in `hofund-plugin-host`:

1. **`BridgeCallbackHandle`** (alongside [`LoopbackHandle`](crates/kernel/plugin-api/src/host.rs:1203+), not a replacement)
   - `register(token, Registration)` → drop guard unregisters.
   - `base_url()` → `http://127.0.0.1:<port>` for writing the callback file.
   - Dedicated listener on `127.0.0.1` only; **no** gateway auth bypass.
   - Handler receives POST body bytes; returns `(status, json_bytes)`.
   - Unknown token → 404 fixed body; constant-time compare over all live tokens; never log request targets.

2. **`BridgeHelperProgram`**
   - `fn bridge_mcp_helper(&self) -> SpawnRequest` — absolute path to current binary + static subcommand argv prefix `bridge-mcp-stdio` (no vendor in API name).

3. **`LongLivedChild`** (extend `PipedChild`; **R-6a**)
   - `write_stdin(Bytes)`, `stdout_stream() -> BodyStream`, explicit `kill()`, `wait() -> ExitStatus`.
   - Spawn: **`env_clear()`** then **only** `SpawnRequest` env pairs (plus minimal host vars such as `PATH` / locale), **not** `piped.rs` `allowed_env()` proxy injection (`61-63`).
   - **Ownership:** the plugin **`Bridge` table** holds the handle across multiple `execute` calls for the same parked run; dropping at the end of one HTTP request must **not** kill the child (`piped.rs:152-166` today kills on drop).
   - **Stdout** line buffer **at least 64 MiB** (`claude_subscription.go:3408-3409`; today `MAX_LINE` is 1 MiB at `piped.rs:22-23`).
   - **Stderr** retained up to **1 MiB**, redacted in logs (`3337-3339`; today stderr is discarded in `piped.rs:5-6`).
   - **`wait()`** reports exit status without implying drop-kill. Reuse the **piped exit status API** from the in-progress **R-8a** kernel slice (`docs/cursor-upstream-design.md` §3.2, §5 table “Piped exit status”) instead of duplicating a second status channel.

4. **`ConcurrencySlot`** (optional if room logic stays plugin-local until host-wide cap)
   - `acquire(label, timeout)` → `SlotGuard`; global across plugin instances in one daemon (reference `claudeRoom`, `claude_room.go:21-47`).

Kernel implementation files (expected): `crates/kernel/plugin-host/src/bridge_callback.rs`, extend `piped.rs`, wire `HostContext::bridge_callback()` in `host.rs`. No vendor names in those files. **No** gateway route or auth bypass for callbacks.

### 5.3 Dependencies

Any new workspace crate edge must update `docs/architecture.md` and `xtask/tests/workspace_invariants.rs`. `cargo deny` with `multiple-versions = deny` — prefer existing `reqwest`/`hyper` in plugin-host only.

## 6. Testing without the real binary

No network. Enforced by:

- **`FakeClaude` script** (shell or Rust) in `hofund-kit-subscription`: reads stdin stream-json lines, writes scripted stdout lines from fixture; asserts env (`CLAUDE_CONFIG_DIR`, `IS_SANDBOX=1` when root, `CLAUDE_CODE_OAUTH_TOKEN` in env not argv, WSL path rewrite on helper and prompt files).
- **Injected runner** trait in plugin `#[cfg(test)]` replacing `program()` and spawn (pattern from existing `cli.rs` test hooks `test_configured_dir`).
- **MCP helper unit tests** porting `claudebridge/mcp_test.go` and `claude_callback_addr_test.go` against hyper/local connector in **plugin-host** tests (not plugin crate).
- **Golden stream fixtures** under `plugins/subscriptions/claude/fixtures/stream/` for `message_delta`, tool_use, `rate_limit_event`, `result` error.
- **Bridge integration tests** in plugin using fake runner + in-memory callback channel (no TCP) for parking: helper thread POSTs to channel simulating host handler.

Mutation tests (must fail on one-char regressions):

- Callback rejects wrong token (`claude_callback_addr_test.go:134`).
- `tools/call` blocks until result POST (`claude_subscription_park_test.go` header comment).
- Resume sends only `since` messages (`1771-1847` logs).
- `match` retry window (`4891-4923`).
- WSL env rewriting when enabled (mock): **`WSLENV`**, root **`IS_SANDBOX`**, proxy vars with loopback host rewrite (`wslrun.go:437-497`).
- Proxy env present when account has proxy metadata (Bun helper only, not host inherit).
- **One-off end → abort, not idle** (`2200-2207`).
- **No-cooldown classification:** sign-in lapse uses **`bridgeFailed` status + `.without_cooldown()`**; third-party **400** and room-full **503** likewise (`gateway.go:4226-4276`, `6432-6441`). Router tests in **R-6g** assert **`skip_failover_same_plugin`** skips same plugin id only.

Keep `plugins/subscriptions/claude/src/conformance.rs` expecting **`upstream()` unsupported** until the slice that completes a **text-only** turn on the fake runner (`lib.rs:509-516`). That first served slice must **fail closed** for images, tools, and sign-in lapse per sections 4 and 7.

## 7. Work breakdown

Slices are ordered; later slices depend on earlier ones. **Safe before further design review:** R-6a (host API + MCP helper tests only), R-6b (plugin scaffold + fake runner; **`upstream()` stays unsupported**).

### R-6a — Host API and MCP helper (kernel/cli)

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `BridgeCallbackBackend` + **127.0.0.1-only** listener | `plugin-api`, `plugin-host` | 400 LOC | Callback reachable off-loopback |
| `LongLivedChild` / piped limits + **`wait()`** (reuse R-8a exit status) | `plugin-api`, `plugin-host` | 200 LOC | Drop-kill ends parked runs |
| Daemon **`UpstreamProvider::shutdown`** at process exit | `hofund-daemon` | 80 LOC | Orphan Claude Code after stop |
| `bridge-mcp-stdio` subcommand (protocol reimplemented from `mcp.go`) | `apps/cli` or daemon | 250 LOC | Drift from Claude Code MCP expectations |
| Host tests | `plugin-host/tests/bridge_*.rs` | 200 LOC | |

Deferred: handover forward (`lan.go:548-551`).

### R-6b — Plugin scaffold + fake runner

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `Bridge` struct stub; **`upstream()` still unsupported** | `plugins/subscriptions/claude/src/{upstream,bridge}.rs`, `lib.rs` | 300 LOC | Premature provider enables real traffic |
| Fake Claude + golden parse tests | `fixtures/`, `bridge/decode.rs` tests | 400 LOC | |

Safe pre-review: types, decode tests, fake runner only — no live `UpstreamProvider` until text-only E2E passes.

### R-6c — Process spawn + stream decode (text only)

| Piece | Files | ~Size | Risk |
|-------|-------|-------|------|
| `startRun` port (no resume) | `bridge/run.rs` | 800 LOC | Windows argv length → system prompt file |
| stream-json → IR text | `bridge/decode.rs` | 500 LOC | Partial message handling |
| WSL + proxy env + root `IS_SANDBOX` | `cli.rs`, `wsl.rs` | 200 LOC | Wrong distro |
| Fake-runner integration tests | `bridge/run.rs` tests | 150 LOC | Env order regressions |

**First slice that may return `upstream()` provider:** completes one **text-only** turn on the fake runner; images/tools/sign-in lapse fail closed.

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
| `failure.rs` classification + **`skip_failover_same_plugin`** in plugin-api/router | `plugins/...`, `plugin-api`, `router` | 250 LOC | Wrong rest → account stuck |
| `docs/coverage/subscription-parity.md`, adopt-or-redesign comparison table per AGENTS.md | docs | | |

### Explicitly deferred (out of scope)

- Sessions recording, Library agent-config writes (roadmap).
- MCP router command execution (R-38).
- Interactive TUI mode (`claudetui`) unless product asks.
- Gateway handover between daemon instances.
- Web search tool injection (reference `6204-6210`) — optional slice after R-6f.

## 8. Document summary (English)

This design specifies how hofund serves Claude Pro/Max subscription traffic by spawning Claude Code with `-p` and `stream-json`, attaching a stdio MCP helper that POSTs tool calls to a **dedicated loopback callback listener**, and keeping long-lived runs with idle reuse keyed by **`turnKey`**, session shelve/resume, and tool-result parking matching the reference bridge. The Claude plugin owns bridge state, child handles, and stream mapping; the kernel gains generic long-lived piped processes and the callback listener; the gateway continues to translate inbound API shapes to IR. Errors map to existing `PluginError` kinds with explicit **`.without_cooldown()`** where the reference passes over accounts without resting them. Testing relies on fake Claude scripts, golden stream fixtures, and host-level MCP tests without network access. Work is split into R-6a through R-6g; **`upstream()` stays unsupported until R-6c** completes a text-only fake-runner turn.

## 9. Open questions

1. ~~**Conversation key in IR**~~ — **Resolved:** idle/resume uses **`turnKey`** over request fields and messages (section 1.7); IR has no session id (`crates/kernel/ir/src/request.rs:16-39`).
2. **Helper placement** — Single `hofundd` subcommand vs tiny dedicated helper binary for faster spawn?
3. **TUI mode** — Is interactive Claude Code required for any shipped agent, or can R-6 ship `-p` only first?
4. **Room limit default** — Port memory-based `claudeMemRuns` or start with a conservative fixed cap?
5. ~~**Callback path on gateway vs sidecar**~~ — **Resolved:** dedicated **127.0.0.1** listener in `plugin-host`; no gateway mux or auth bypass.
