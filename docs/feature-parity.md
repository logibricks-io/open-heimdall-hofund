# Feature parity: magpie and hofund

This document compares every user-facing capability of
[yetone/magpie](https://github.com/yetone/magpie), the Go original, with
hofund, the Rust rewrite. Each row cites a source path in each repository.

- **magpie snapshot:** `641b121` (shallow clone, October 2026).
- **hofund snapshot:** `main` at `48dbd71` ("desktop tray app and release pipeline", #8).
  Hofund line citations below were checked against that commit on 2026-10-25.
- **Blocker pull requests, not yet merged:** [#9](https://github.com/logibricks-io/open-heimdall-hofund/pull/9)
  (`4f7b3c0`, codec correctness) and [#10](https://github.com/logibricks-io/open-heimdall-hofund/pull/10)
  (`408ca18`, CLI and agent UX). Rows those pull requests finish are marked Done and cite the pull request.
  `main` does not contain that code until the pull requests merge.
- **Goal:** rebuild every core magpie capability in Rust and do each one better.
  Copy designs that are good, stable, and efficient. Redesign the parts that are unsound.
  `docs/magpie-comparison.md` records each adopt-or-redesign verdict.
  `docs/roadmap.md` is the execution order. A `Stage` column marks each row `core` (stage 1) or `auxiliary` (stage 2).

## Status key

| Status | Meaning |
| --- | --- |
| Done | hofund has the capability, with equal or wider scope for the user. |
| Partial | hofund has a subset, or it has a known defect that blocks real use. |
| Missing | hofund has nothing for it. |
| Not planned | hofund leaves it out on purpose. The reason is in the row. These rows are not counted. |

| Stage | Meaning |
| --- | --- |
| core | Stage 1. Required before auxiliary work starts. |
| auxiliary | Stage 2. Do not start until stage 1 has merged. |

Unless a path says otherwise, magpie paths are relative to the magpie repository
and hofund paths to the hofund repository. `ref.md:N` means magpie's
`docs/reference.md` at line N.

## Summary

| Status | Rows |
| --- | --- |
| Done | 20 |
| Partial | 19 |
| Missing | 115 |
| Total counted | 154 |
| Not planned (not counted) | 1 |

| Stage | Counted rows |
| --- | --- |
| core | 77 |
| auxiliary | 77 |

### Where hofund is already better

1. **Auth on loopback too.** The gateway token is required on loopback as well
   (`crates/gateway/src/lib.rs`, `authorized`), and tokens are compared in constant time.
   magpie accepts any key on loopback (`ref.md:1920`, "any value works").
   That leaves it open to DNS rebinding and to any local process.
2. **Hub-and-spoke IR.** Each protocol has one codec to and from a shared IR
   (`crates/ir`, `crates/codec`). That is N codecs where pairwise translation needs N×N.
   magpie translates in many protocol-specific files under `internal/gateway`
   (`chat.go`, `responses.go`, `anthropic.go`, `gemini.go` and over 190 others).
3. **Quirks are data.** Vendor workarounds are declarative rules in `crates/quirks`.
   magpie scatters them through per-vendor files (`deepseek_schema.go`, `kimi_schema.go`,
   `grok_schema.go`, `zhipu_envelope.go`).
4. **No vendor names in core code.** A test bans vendor names in the gateway and router
   crates, and dependencies flow one way between crates (`docs/architecture.md`).
5. **Secrets out of plain files.** API keys can be `env:NAME` references
   (`crates/secrets/src/lib.rs`). magpie keeps keys in `providers.json` (`ref.md:2998`, "Files").
6. **Backpressure.** Streams are pulled through the codec, not buffered (`crates/provider/src/stream.rs`).
7. **Agent backup markers.** Backups use their own markers (`*.hofund.bak`, `*.hofund.absent`)
   and live beside the file (`crates/agent/src/catalog.rs`). PR #10 disconnects by deleting
   only the keys connect wrote, so a later edit stays. `--force` restores the backup bytes.
8. **One config file.** A single TOML file (`crates/config/src/lib.rs`) replaces about 20
   JSON and YAML state files (`ref.md:2998`). It can be reviewed, diffed and checked
   (`hofund config check`).
9. **Release pipeline.** cargo-dist builds 5 targets plus a universal2 macOS binary,
   with checksums and optional signing (`dist-workspace.toml`, `.github/workflows/release*.yml`).
   That is reproducible from CI.

## Matrix

### 1. Gateway protocols and endpoints

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 1.1 | OpenAI Chat Completions endpoint `/v1/chat/completions` | `internal/gateway/chat.go`; `ref.md:1880` | core | Done | `crates/codec/src/openai_chat.rs`; PR #9 sets `stream_options.include_usage` on streamed Chat requests | PR #9 sends `stream_options.include_usage` so a streamed Chat upstream reports usage. Next: golden stream tests per vendor. |
| 1.2 | OpenAI Responses endpoint `/v1/responses` (SSE) | `internal/gateway/responses.go`; `ref.md:1882` | core | Done | `crates/codec/src/openai_responses.rs`; PR #9 | PR #9 emits `output_text.done` and `content_part.done`, puts `id` and `content` on `output_item.done`, puts the full `output` array on `response.completed`, and sets `sequence_number` on every event. |
| 1.3 | Anthropic Messages endpoint `/v1/messages` | `internal/gateway/anthropic.go`; `ref.md:1883` | core | Done | `crates/codec/src/anthropic.rs`; `crates/provider/src/http.rs`; PR #9 | PR #9 puts usage on `message_start`, emits `content_block_stop` before a single `message_delta`, and prefixes message ids with `msg_`. Extension fields pass through only when the inbound and outbound protocols match, so `metadata.user_id` and `cache_control` do not cross protocols. |
| 1.4 | Gemini `generateContent` and `streamGenerateContent` | `internal/gateway/gemini.go`; `ref.md:1885` | core | Done | `crates/codec/src/generative.rs`; `crates/gateway/src/endpoints.rs` | Keep. Add `countTokens` (row 1.5). |
| 1.5 | Token counting: `/v1/messages/count_tokens` and Gemini `:countTokens` | `ref.md:1884-1885` | core | Missing | none | Forward to the upstream when it supports counting. Otherwise estimate with a tokenizer chosen by model family and mark the result as an estimate. |
| 1.6 | Model list `/v1/models` and `/v1beta/models`, with reasoning levels and context sizes | `ref.md:1887`, `ref.md:1890` | core | Partial | `crates/gateway/src/lib.rs:162` (ids only) | Add context window, output limit, modalities and reasoning levels from the model catalog (row 2.8). |
| 1.7 | Codex model catalog `/v1/codex/models` | `internal/gateway/codex_catalog.go`; `ref.md:1888` | core | Missing | none | Render from the same catalog as row 1.6, in one codec-side formatter. |
| 1.8 | Ollama-compatible listing `/api/version`, `/api/tags`, `/api/show` | `internal/gateway/ollama.go`; `ref.md:1889` | auxiliary | Missing | none | Low cost. Add as an optional listing codec. |
| 1.9 | Speech to text passthrough `/v1/audio/transcriptions` and `/translations` | `internal/gateway/transcribe.go`; `ref.md:1886` | auxiliary | Missing | none | Multipart passthrough with model rewrite only. Stream the body instead of buffering it. |
| 1.10 | Embeddings | `internal/gateway/embeddings.go` | auxiliary | Missing | none | Add an `/v1/embeddings` codec over the same router. |
| 1.11 | Routes without the `/v1` prefix and behind a path-prefixed reverse proxy | `ref.md:1864-1866` | auxiliary | Missing | `crates/gateway/src/lib.rs:95` | Add a configurable base path. Never guess from headers. |
| 1.12 | Cross-protocol translation with tools, reasoning and images | `internal/gateway/ir.go`, `chat_tool_shape.go`, `image_input.go` | core | Done | `crates/ir/src/content.rs`; `crates/codec` | Already better (hub IR). Add a translation conformance suite over every pair. |
| 1.13 | Streaming with flow control | `internal/gateway/sse.go`, `whole_stream.go` | core | Done | `crates/provider/src/stream.rs` | Already better (backpressure). |
| 1.14 | Reasoning effort mapped between wires (`effort`, `reasoning_effort`, `thinking.budget_tokens`) | `internal/gateway/wire_effort.go`, `effort_update.go`; `ref.md:1291` | core | Missing | `crates/ir` holds thinking parts only; no effort field | Add an IR effort enum with one mapping table per codec. Clamp to the levels the model catalog lists. |
| 1.15 | Prompt cache handling (Anthropic `cache_control`, cache stays on one account) | `internal/gateway/claude_cache.go`; `ref.md:1600-1616` | core | Partial | cache tokens are parsed in `crates/codec/src/*.rs` but not stored | Same-protocol requests keep `cache_control` on the extension bag. PR #9 drops it when the outbound protocol differs. Store cache tokens (row 9.2) and keep a session on one upstream (row 4.6). |
| 1.16 | Context compaction for Codex (native and summary) | `internal/gateway/native_compaction.go`, `compacting.go`; `internal/codexsummary` | auxiliary | Missing | none | Pass native compaction through. Add summary compaction only behind a flag. |
| 1.17 | Vendor JSON-schema fixes for tools | `internal/gateway/deepseek_schema.go`, `kimi_schema.go`, `grok_schema.go`, `oneof_schema.go` | auxiliary | Partial | `crates/quirks/src/lib.rs` (5 rules, none for schemas) | Add schema-rewrite quirk rules as data, with one fixture per vendor. |
| 1.18 | `max_tokens` default when a client omits it | `internal/gateway/maxoutput.go`; `ref.md:1420` | core | Done | `crates/quirks/src/lib.rs` (`DefaultMaxTokens`) | Take the default from the model's output limit in the catalog instead of a fixed number. |
| 1.19 | Tool-call repair: text tool calls, undeclared tools, tool loop guard | `internal/gateway/textcalls.go`, `undeclared_tools.go`, `loop_guard.go`, `tool_loop.go` | auxiliary | Missing | none | Add as opt-in quirk rules, with metrics for how often each one fires. |
| 1.20 | Bare model ids and `provider/model` names | `internal/gateway/unknown_model.go`; `internal/provider/bare.go`; `ref.md:1924` | core | Partial | `crates/router/src/lib.rs:81` (exact model name only) | Accept `provider/model`, a route alias, or a bare id that exactly one provider serves. Give a clear error naming candidates otherwise. |
| 1.21 | CORS for browser clients | `internal/gateway/cors.go` | auxiliary | Missing | none | Off by default. Allow listed origins only. |

### 2. Upstream providers and models

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 2.1 | OpenAI-compatible upstreams (Chat and Responses) | `internal/provider/provider.go` | core | Done | `crates/provider/src/openai_compat.rs`; `crates/config/src/lib.rs` (`OpenAiCompat`) | Keep. |
| 2.2 | Anthropic upstream | `internal/provider/provider.go` | core | Done | `crates/provider/src/anthropic.rs` | Keep. |
| 2.3 | Google Gemini API upstream | `internal/provider/gemini_api.go`, `google.go` | core | Missing | `crates/provider` has no Gemini adapter | Add a Gemini adapter over the existing generative codec. |
| 2.4 | Google Vertex AI (ADC, impersonation, per-location models) | `internal/provider/vertex.go`, `internal/gateway/vertex.go`; `ref.md:867` | core | Missing | none | Add with `gcp_auth`-style ADC. Store no service account key. |
| 2.5 | AWS Bedrock (Converse and Mantle) | `internal/provider/presets.go` (`bedrock`, `bedrock-mantle`); `internal/gateway/mantle.go` | core | Missing | none | Add with SigV4 signing and the standard AWS credential chain. |
| 2.6 | Azure OpenAI | `internal/provider/azure.go` | core | Missing | none | A quirk profile for path, `api-version` and `api-key` on the OpenAI adapter. |
| 2.7 | Provider presets (94 entries: vendors, coding plans, regions, local servers) | `internal/provider/presets.go`; `README.md` | core | Missing | none | Ship presets as a versioned data file (TOML), not code. Test each preset's endpoints in CI. |
| 2.8 | Model catalog from models.dev: names, prices, limits, reasoning levels | `internal/catalog`; `ref.md:1291` | core | Missing | none | Cache the catalog on disk with an ETag, allow a mirror, and let local overrides win. |
| 2.9 | Live model lists from each provider, with scheduled refresh and auto-include | `internal/provider/autolist.go`, `modellist.go`; `ref.md:319` | core | Missing | none | Refresh in the background with a backoff. Show a diff of what was added or removed. |
| 2.10 | Provider test and endpoint detection | `internal/provider/test.go`, `detect.go`; `internal/probelog` | core | Missing | none | Add `hofund provider test <id>`, which sends a minimal request per protocol and logs it like any other request. |
| 2.11 | Custom headers and User-Agent per provider | `internal/provider/authheader.go`, `user_agent.go`; `ref.md:340` | core | Done | `crates/config/src/lib.rs` (`headers`) | Keep. |
| 2.12 | Several keys per provider, with rotation and per-key rest | `internal/provider/keys.go`; `ref.md:1560` | core | Missing | `crates/config/src/lib.rs` (one `api_key`) | Model keys as router members, so all routing strategies apply to keys too. |
| 2.13 | Model alias and wire name (the name a client sends differs from the vendor's) | `internal/provider/usagealias.go`, `rename.go`; `ref.md:1367` | core | Missing | `crates/config/src/lib.rs:89` (`Route` maps a model to provider ids; the model name is sent unchanged) | Let each route member name its own upstream model: `upstreams = [{provider, model}]`. This is what makes cross-vendor failover work. |
| 2.14 | Model spec: context window, output limit, vision, reasoning levels | `internal/provider/modelspec.go`; `ref.md:1291` | core | Missing | none | Store the spec in the catalog (row 2.8) and use it in routing and quirks. |
| 2.15 | Hide deprecated and retired models; disable a model | `internal/provider/retired.go`; `internal/gateway/disabled.go` | core | Missing | none | Add flags in the catalog. |
| 2.16 | Extra body fields per provider or model | `internal/provider/extrabody.go` | auxiliary | Missing | none | A `set` and `unset` quirk rule with JSON-pointer paths. |
| 2.17 | Import providers from CC Switch, Claude Code, Codex, OpenCode and Alma | `internal/provider/importapps.go`, `import_codex.go`, `import_opencode.go`; `ref.md:1520` | auxiliary | Missing | none | Read-only importers that produce a TOML diff to review before writing. |
| 2.18 | `magpie://import` links | `internal/provider/importurl.go`; `ref.md:2286` | auxiliary | Missing | none | Register a `hofund://` scheme in the desktop app. Always confirm and show the diff. |
| 2.19 | Local servers (Ollama, LM Studio) | `internal/provider/presets.go` (`ollama`, `lmstudio`) | core | Partial | works as an `OpenAiCompat` provider in config | Add presets and local detection. |
| 2.20 | Vendor status pages taken into account | `internal/upstream` | auxiliary | Missing | none | Poll status pages, and show "vendor degraded" instead of a sign-in error. |
| 2.21 | Allow insecure TLS for one provider | `internal/provider/insecure_tls.go` | auxiliary | Missing | none | A per-provider custom CA option first. Insecure mode only with a loud warning. |

### 3. Subscriptions and accounts

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 3.1 | Codex / ChatGPT subscription as a provider | `internal/gateway/codex_backend.go`; `internal/provider/codex_*.go`; `ref.md:1779` | core | Missing | none | One `Subscription` trait: sign-in, refresh, quota and model list. Each vendor is an adapter crate behind a feature flag. |
| 3.2 | Claude subscription by driving the local `claude` binary over an MCP bridge | `internal/claudebridge`; `internal/gateway/claude_subscription.go`; `docs/subsystems/claude-subscription-bridge.md` | core | Missing | none | A normal provider the user adds in config, with the same credential storage, refresh, and quota path as the other subscription adapters. Provider docs state, in one neutral sentence, that subscription use follows the vendor's terms. Not a hidden opt-in. |
| 3.3 | GitHub Copilot subscription, including Enterprise | `internal/provider/copilot_*.go` | core | Missing | none | Adapter on the trait from row 3.1. |
| 3.4 | Gemini Code Assist and Antigravity | `internal/gateway/codeassist.go`; `internal/provider/google.go`, `antigravity_*.go` | core | Missing | none | Adapter. Warn about account suspension risk, as magpie does. |
| 3.5 | Other sign-ins: Grok, Cursor, Devin, Kiro, Qoder, Factory, Zed, WorkBuddy, Z Code, Cline | `internal/provider/grok.go`, `cursor.go`, `devin.go`, `kiro.go`, `qoder.go`, `factory.go`, `zed.go`, `workbuddy.go`, `zcode.go`, `cline.go` | core | Missing | none | Community adapters behind the same trait, maintained out of the core crates. |
| 3.6 | Account discovery from agents' own sign-ins (`MAGPIE_NO_ACCOUNTS=1` turns it off) | `internal/provider/discovery.go`; `README.md` | core | Missing | none | Opt-in, not opt-out. Read only. Never write tokens back without asking. |
| 3.7 | Several accounts per subscription; import accounts from files | `internal/provider/login_import.go`, `logins.go`; `ref.md:1830` | core | Missing | none | Accounts as router members (as in row 2.12). |
| 3.8 | Quota and allowance windows; `magpie quota`, `quota wait`, history | `internal/provider/quotas.go`, `quota_history.go`; `ref.md:2588` | core | Missing | none | One quota model (windows, resets, credits) shared by routing, the UI and the CLI. |
| 3.9 | Key balances and plan windows for relays and coding plans | `internal/provider/balance.go`, `planquota.go`, `sub2api_usage.go` | core | Missing | none | Balance URL per provider, using the same quota model. |
| 3.10 | Quota and usage alerts | `internal/provider/quota_alert.go` | core | Missing | none | Desktop notification and a CLI warning at configurable thresholds. |
| 3.11 | Daily warm-up to start allowance windows early | `internal/provider/warmup_daily.go`; `plugins/warmups` | core | Missing | none | Opt-in, with the cost shown in usage. |
| 3.12 | Daily check-in for vendors that reward it | `internal/provider/checkin.go`, `*_checkin.go` | core | Missing | none | Low priority. Plugin only. |

### 4. Routing, failover and load balancing

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 4.1 | Ordered failover before the first byte | `internal/gateway/fallback.go`; `docs/subsystems/gateway-routing.md` | core | Done | `crates/router/src/lib.rs` | Keep. |
| 4.2 | Configurable failure classes (connect errors, statuses) | `docs/subsystems/gateway-routing.md` | core | Done | `crates/config/src/lib.rs` (`failover.connect_errors`, `failover.statuses`) | Already better: explicit and typed. |
| 4.3 | Named routing groups (`group/<id>`), nested groups, copy | `internal/provider/group*.go`; `plugins/groups`; `ref.md:1433` | core | Partial | `crates/config/src/lib.rs:89` (`Route` is an unnamed ordered group) | Make a group a first-class node in a routing tree. Reject cycles when config loads. |
| 4.4 | Groups found automatically for a model that several providers serve | `internal/provider/group_serve.go`; `ref.md:1924` | auxiliary | Missing | none | Derive from the catalog. Show the result in `hofund routes`. |
| 4.5 | Strategies: smart, order, rotate, least used, weekly pace, weight | `plugins/rotation`; `internal/gateway/weighted.go`; `internal/provider/window_pace.go`; `ref.md:1452` | core | Missing | `docs/magpie-comparison.md` ("only ordered failover") | A `Strategy` trait with pure, unit-tested ordering functions over a snapshot of state. Start with order, weight, round-robin and least used. Add quota-aware ones after row 3.8. |
| 4.6 | Session stickiness for prompt cache (`stays=auto/session/turn/off/strict`) | `internal/gateway/affinity.go`, `session_route.go`; `ref.md:1600` | core | Missing | none | Key on a session id taken from each protocol. Keep the table in memory and persist it in SQLite. |
| 4.7 | Rest and cooldown: `Retry-After`, reset times, doubling backoff, caps | `docs/reference.md:1700-1745`; `internal/gateway/patience.go` | core | Missing | none | A circuit breaker per member, with the times visible in the routing view. |
| 4.8 | Routing rules: tokens, images, effort, agents, time, days, folders, compact | `internal/provider/grouprule.go`, `ruleparse.go`; `internal/cli/groups_rules_cli.go` | auxiliary | Missing | none | Rules as a typed expression in TOML, checked when config loads, with a `hofund route explain` command. |
| 4.9 | Intent routing with a classifier model | `internal/gateway/classify.go`, `decide.go` | auxiliary | Missing | none | Optional, with a time limit and a cache. Fail open to the default order. |
| 4.10 | Move to a larger context window at 95% fill | `ref.md:1400` | auxiliary | Missing | none | Use the catalog spec (row 2.14). |
| 4.11 | Ask another member after a safety refusal | `internal/gateway/strict_refused.go`; `ref.md:1750` | auxiliary | Missing | none | Configurable count, off by default. |
| 4.12 | Fast mode and effort per group member | `internal/gateway/groupfast.go`; `internal/provider/groupeffort.go`, `groupfast.go` | auxiliary | Missing | none | Member-level overrides in the routing tree. |
| 4.13 | Routing view: who went first and why; `/v1/magpie/route` | `internal/gui/assets/plugins/routing`; `internal/gateway/session_route.go` | core | Missing | none | Record a routing decision per request and serve it over a local API (row 12.3). |
| 4.14 | Concurrency and RPM limits | `internal/gateway/concurrency.go`, `rpm.go` | auxiliary | Missing | none | A token bucket per member and per key, built on `tower` layers. |
| 4.15 | Decision models at `/v1/systemone` | `internal/gateway/decide.go`; `ref.md:1494` | auxiliary | Missing | none | Low priority. |
| 4.16 | List only the group's models to clients (`listed=no`) | `ref.md:1486` | auxiliary | Missing | none | A `listed` flag per route. |

### 5. Agent wiring

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 5.1 | Supported agents (about 56 adapters; the README says 65+) | `internal/agent`; `ref.md:88` | core | Partial | `crates/agent/src/catalog.rs` (claude-code, codex, gemini-cli, opencode) | Describe each adapter as data (file, format, keys), so most agents need no code. Add the next 10 by usage. |
| 5.2 | Connect and disconnect with backup and restore | `docs/subsystems/agent-wiring.md`; `internal/agent` | core | Done | `crates/agent/src/catalog.rs` (`disconnect`); PR #10 | PR #10 removes only the keys connect wrote, on the current file, so a later edit such as Codex `sandbox_mode` stays. `--force` restores the connect-time backup. A second disconnect exits 0. Empty directories connect created are removed. |
| 5.3 | Format-preserving edits (TOML, JSONC, YAML) | `internal/edit` | core | Partial | TOML keeps comments; JSONC comments are dropped until revert (`crates/agent/src/catalog.rs`) | Use a lossless JSONC editor that keeps comments. |
| 5.4 | Drift detection | `docs/subsystems/agent-wiring.md` | core | Partial | `crates/agent/src/lib.rs:157` (`ConnectionStatus`) | Report which keys drifted, and offer to fix them. |
| 5.5 | Set an agent's model and effort from the CLI (`magpie claude opus`) | `internal/cli/main.go:38`; `ref.md:2597` | core | Missing | none | `hofund agent set <agent> model=<id> effort=<level>`, checked against the catalog. |
| 5.6 | Profiles: save and switch every agent's settings | `internal/profile`; `ref.md:2597` | core | Missing | none | Snapshots stored as TOML that are easy to diff. |
| 5.7 | Visible model list per agent | `internal/provider/visible.go`; `ref.md:340` | core | Missing | none | A per-agent filter in config. |
| 5.8 | Agent CLI version check and update | `internal/proc`; `internal/provider/install.go` | auxiliary | Missing | none | Report only. Never update without asking. |
| 5.9 | WSL wiring from Windows | `internal/wslrun`; `internal/sessions/wsl.go` | auxiliary | Missing | none | Later phase. |
| 5.10 | Status line for agents (`magpie statusline`) | `internal/cli/main.go:38` | auxiliary | Missing | none | A fast CLI that reads the local API. |

### 6. Library

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 6.1 | Instruction sets written between markers in each agent's file | `internal/library`; `docs/subsystems/library.md` | auxiliary | Missing | none | Reuse the agent adapter data (row 5.1), so the library needs no per-agent code. |
| 6.2 | MCP servers written into each agent in its own format | `docs/subsystems/library.md` ("MCP servers") | auxiliary | Missing | none | Same as row 6.1. |
| 6.3 | Skills (GitHub, git, local, skills.sh market), updates, groups | `docs/subsystems/library.md` ("Skills") | auxiliary | Missing | none | Pin each skill to a commit and verify a hash before linking. |
| 6.4 | Hooks and subagents written into agents | `docs/subsystems/library.md` ("Hooks", "Subagents") | auxiliary | Missing | none | Later phase. |
| 6.5 | RTK install and removal | `docs/subsystems/library.md` ("RTK") | auxiliary | Missing | none | Later phase. |

### 7. Built-in tools and MCP

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 7.1 | Web search for any model (Tavily, Brave, Exa, Firecrawl, You.com, SearXNG and others), several keys | `internal/gateway/search*.go`; `README.md` | auxiliary | Missing | none | One `SearchBackend` trait with an ordered list and a key pool. |
| 7.2 | Web search and fetch as an MCP server (`web_search`, `web_fetch`) | `internal/gateway/search_mcp.go`; `docs/subsystems/gateway-routing.md:642` | auxiliary | Missing | none | Serve MCP over streamable HTTP from the gateway, behind the same token. |
| 7.3 | Image recognition as an MCP server | `internal/gateway/vision_mcp.go`; `gateway-routing.md:700` | auxiliary | Missing | none | Later phase. |
| 7.4 | Image generation MCP (`magpie mcp image`) and video | `internal/imagemcp`; `internal/gateway/draw*.go`, `video.go` | auxiliary | Missing | none | Later phase. |
| 7.5 | Plugin tools offered to every agent as one MCP server | `plugins/tools`; `docs/subsystems/plugin-tools.md` | auxiliary | Missing | none | Comes with the plugin runtime (row 8.1). |
| 7.6 | Sign-in to remote MCP servers that use OAuth | `internal/mcpauth` | auxiliary | Missing | none | Store tokens in the OS keychain. |

### 8. Plugins and middleware

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 8.1 | Gateway middleware hooks `onRequest`, `onEvent`, `onResponse` (JavaScript on Bun) | `internal/middleware`; `docs/subsystems/gateway-middleware.md` | auxiliary | Missing | none | Run plugins as WebAssembly components (wasmtime) over the IR: sandboxed, fast, no Bun download, with fuel and memory limits. |
| 8.2 | Ready-made middleware: param-override, model-map, system-prompt, word-guard, think-tags | `README.md`; `ref.md:1044` | auxiliary | Partial | `crates/quirks/src/lib.rs` (temperature, top-p, max-tokens rules) | Build these in Rust as declarative rules, so they need no plugin at all. |
| 8.3 | Asynchronous request hooks (context recall) | `docs/subsystems/gateway-request-hooks.md` | auxiliary | Missing | none | Run with a time limit and fail open. |
| 8.4 | Built-in plugin registry with on and off switches (`plugins.yml`) | `internal/pluginrt`; `docs/subsystems/plugin-runtime.md` | auxiliary | Missing | none | Use Cargo features and config sections instead of runtime toggles where possible. |
| 8.5 | OpenCode provider plugins and pi packages | `internal/plugin`; `ref.md:957` | auxiliary | Missing | none | Not core. Revisit after row 8.1. |
| 8.6 | Agent plugins that add agents | `internal/agentplug`; `ref.md:1109` | auxiliary | Missing | none | Covered by data-driven adapters (row 5.1). |

### 9. Usage, cost and sessions

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 9.1 | Ledger of every request | `internal/usage`; `usage.jsonl` (`ref.md:2998`) | core | Partial | `crates/usage/src/lib.rs` (SQLite: model, upstream, tokens, latency, status) | Already a better store (SQLite instead of JSONL). Add the fields below, with migrations. |
| 9.2 | Cache read and write tokens (5-minute and 1-hour split), reasoning tokens | `ref.md:1220`; `internal/usage/block_price.go` | core | Missing | `crates/usage/src/lib.rs` has no cache columns | Carry them through IR usage (`crates/ir/src/usage.rs`) into the ledger. |
| 9.3 | Cost at effective prices: custom, provider catalog, models.dev; USD or CNY | `internal/catalog`; `internal/fx`; `ref.md:1150` | core | Missing | none | Store the price used in each row, so totals do not change when prices change. Mark calls with no price as unpriced, as magpie does. |
| 9.4 | Usage report by period and CSV export | `internal/usageapi`; `internal/cli/main.go:38` | core | Partial | `crates/cli/src/main.rs` (`usage`); `crates/daemon/src/lib.rs` (`usage_report`) | Add `--since`, `--group-by`, `--csv` and `--json`. |
| 9.5 | Attribution by agent, provider key and gateway key | `internal/usage/client_agents.go`; `ref.md:340` | core | Missing | none | Detect the agent from the path, headers and gateway key. Record it per row. |
| 9.6 | Sessions with cost and title; grouping of subagents | `internal/sessions`; `docs/subsystems/gateway-sessions.md` | auxiliary | Missing | none | A session id from each protocol, stored in the ledger. |
| 9.7 | Read usage from agents' own session files | `internal/sessions/calls_*.go` | auxiliary | Missing | none | Later phase. |
| 9.8 | Apps report their own usage (`POST /v1/magpie/usage/requests`) | `internal/gateway/usage_ingest.go`; `ref.md:2096` | auxiliary | Missing | none | Idempotent on a request id, as magpie does. |
| 9.9 | Output speed and time to first token | `internal/gateway/ttft.go`; `docs/subsystems/gui-shell.md` ("Streamed output speed") | core | Partial | `crates/usage/src/lib.rs` (`latency_ms` only) | Record time to first byte and tokens per second. |
| 9.10 | Usage heatmap and summary API (`/v1/magpie/usage`) | `internal/usage/heatmap.go`; `internal/gateway/usage_api.go` | auxiliary | Missing | none | Serve from SQLite queries. |
| 9.11 | Settings advice from real usage (compaction, cache TTL) | `internal/tune` | auxiliary | Missing | none | Low priority. |
| 9.12 | Gateway key limits per day, week and month (`/v1/magpie/limit`) | `internal/budget`; `internal/gwbudget`; `ref.md:1440` | core | Missing | none | Enforce in SQLite transactions, so limits hold across restarts. |

### 10. Auth, sharing and network

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 10.1 | Gateway authentication | `internal/gateway/lan.go`; `ref.md:1920` | core | Done | `crates/gateway/src/lib.rs:288` (`authorized`); `crates/secrets/src/lib.rs` (`TokenFile`, mode 0600) | Already better: a token is required on loopback too. |
| 10.2 | Accept Bearer, `x-api-key`, `x-goog-api-key` or `?key=` | `ref.md:1440` | core | Done | `crates/gateway/src/lib.rs` (`authorized`); PR #10 | PR #10 accepts `x-api-key` and compares it in constant time, with the bearer token, `x-goog-api-key`, and a `key` query value. |
| 10.3 | Token rotation | `internal/access` | core | Done | `crates/gateway/src/lib.rs` (`current_token`); `crates/cli/src/main.rs` (`token rotate`); PR #10 | PR #10 re-reads the token file on each request and keeps the last good token when the read fails. Connected agents have their token keys rewritten. A failed rewrite prints `hofund agent connect <id>`. |
| 10.4 | Named gateway keys with model and account limits | `internal/access`; `caller-keys.json` (`ref.md:2998`) | core | Missing | `crates/secrets/src/lib.rs` (one token) | Store hashed keys only (magpie stores them readable), with scopes and limits. |
| 10.5 | LAN sharing, allowed IPs (`MAGPIE_ALLOW_IPS`), trusted proxy (`MAGPIE_TRUST_PROXY`) | `internal/gateway/lan.go`, `allow.go`; `ref.md:1850` | core | Partial | `crates/config/src/lib.rs` (`listen` can be any address) | Require named keys for any address other than loopback, plus a CIDR allow-list. |
| 10.6 | Failed-auth throttling | `internal/gateway/auththrottle.go` | auxiliary | Missing | none | Throttle per IP with a `tower` layer. |
| 10.7 | TLS serving with certificate hot reload | `internal/gateway/tls.go`; `ref.md:1440` | auxiliary | Missing | none | rustls, with the certificate reloaded when its file changes. |
| 10.8 | Public URL for reverse proxies (`MAGPIE_PUBLIC_URL`) | `ref.md:1843` | auxiliary | Missing | none | A config field used in printed examples. |
| 10.9 | Remote magpie as a provider | `internal/gateway/remote_magpie.go`; `internal/provider/remote_magpie.go` | auxiliary | Missing | none | A remote hofund is just an OpenAI or Anthropic provider with a named key. Add quota pass-through later. |
| 10.10 | Outbound proxy for vendor requests: global and per provider (http, https, socks5) | `internal/netproxy`; `internal/provider/proxy.go` | auxiliary | Partial | reqwest honors `HTTPS_PROXY` from the environment (`crates/provider/Cargo.toml`) | Add proxy settings globally and per provider, including socks5. |
| 10.11 | Secrets kept out of config files | `providers.json`, mode 0600 (`ref.md:2998`) | core | Done | `crates/secrets/src/lib.rs` (`env:NAME`) | Already better. Next, add OS keychain references (`keychain:NAME`). |

### 11. Configuration and state

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 11.1 | Starter config and validation | `internal/appdir`; `internal/settings` | core | Done | `examples/hofund.toml`; `crates/cli/src/main.rs`; PR #10 | PR #10 adds `--help`, `-h`, and `help`, prints the config path from `config check`, and ships `examples/hofund.toml` with `[[providers]]` and `[[routes]]`. Next: point a validation error at the exact key. |
| 11.2 | Changes apply without a restart | `internal/filememo`; app edits apply live | core | Missing | none | Watch the config file and swap the router atomically (`arc-swap`). Keep running requests on the old config. |
| 11.3 | Atomic writes, cross-process file locks, last-good fallback | `internal/filelock`, `internal/steady`, `internal/lastgood` | auxiliary | Partial | atomic writes in `crates/agent` and `crates/secrets` | Add advisory locks and keep the last good config. |
| 11.4 | Environment overrides (`MAGPIE_ADDR` and others) | `ref.md:1843` | auxiliary | Partial | `crates/cli/src/main.rs` (`--config` only) | `HOFUND_` variables for listen, config and log level. |
| 11.5 | Config surgery from the CLI (`provider add`, `group set`, `key`) | `internal/cli`; `ref.md:319` | core | Missing | none | Edit commands that keep comments in `hofund.toml`, built on `toml_edit`. |

### 12. Observability

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 12.1 | Recent request inspection with bodies | `internal/gateway/reqlog.go`, `capture.go`; `docs/subsystems/gui-shell.md` ("Recent-call bodies") | auxiliary | Missing | none | A bounded ring buffer, off by default, with secrets redacted before storage. |
| 12.2 | OTLP traces and metrics export | `internal/otlp`; `plugins/otel`; `ref.md:2782` | auxiliary | Missing | no `tracing` dependency | Use `tracing` with `opentelemetry-otlp` behind a feature. Follow the GenAI semantic conventions. |
| 12.3 | Local management API (`/v1/magpie/*`: quotas, usage, route, concurrency) | `ref.md:1855-1870` | core | Missing | none | One versioned `/hofund/v1` API, used by the desktop app, the TUI and the CLI. |
| 12.4 | Runtime profiling (heap, CPU, goroutines; `healthcheck`) | `internal/gateway/heap.go`, `cpu.go`, `goroutines.go`; `internal/cli/main.go:38` | core | Partial | PR #10 `GET /health` (no auth, body `{"status":"ok"}`) | PR #10 adds `GET /health` with no auth and no secrets. Still open: `hofund healthcheck` and `tokio-console` behind a feature. |
| 12.5 | Structured logs | `internal/probelog`; `internal/gateway/trace.go` | auxiliary | Missing | none | `tracing` with JSON output and a request id on every line. |

### 13. Privacy masking

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 13.1 | Rule-based masking with placeholders restored in streams and tool arguments | `internal/redact`; `internal/masking`; `README.md` | auxiliary | Missing | none | Run as an IR pass on requests and a streaming restore pass on replies. Fuzz it across chunk boundaries. |
| 13.2 | Optional local PII model (about 1.6 GB) | `internal/pii`; `native/privacy-filter` | auxiliary | Missing | none | Later. An ONNX model behind a feature, downloaded only on request. |
| 13.3 | Mask, Warn and Block modes; never-mask list; return 503 when masking fails | `README.md`; `internal/maskrec` | auxiliary | Missing | none | Fail closed, as magpie does. Add an audit log of what was masked, with no values. |

### 14. Backup and sync

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 14.1 | Encrypted backup and restore with a passphrase | `internal/backup`; `ref.md:2672` | auxiliary | Missing | none | `age` encryption over the TOML config and SQLite. |
| 14.2 | Sync over WebDAV, S3 and an encrypted GitHub repository | `internal/davsync`; `docs/subsystems/sync-backup.md`; `ref.md:2710` | auxiliary | Missing | none | Later phase. Config is one file, so sync is far simpler than in magpie. |

### 15. Desktop app and user interfaces

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 15.1 | Menu bar tray | `internal/gui`; `magpie tray` | core | Partial | `apps/desktop/src-tauri/src/lib.rs` (start, stop, agent toggles, usage, open config, quit) | Add status, current route and quota in the menu. |
| 15.2 | App window with pages: Agents, Providers, Routing, Usage, Sessions, Library, Masking, Plugins, Gateway, Settings | `internal/gui/assets/index.html` (`data-view`); `docs/subsystems/gui-shell.md` | auxiliary | Missing | `apps/desktop/ui` (tray only) | Tauri window over the local API (row 12.3). Pages ship in impact order: Providers, Routing, Usage, Agents. |
| 15.3 | Terminal UI (`magpie tui`) | `internal/tui` | core | Missing | none | `ratatui` over the same local API. |
| 15.4 | Browser UI (`magpie web`, `--lan`) | `internal/cli/main.go:38`; `ref.md:2380` | auxiliary | Missing | none | Serve the same frontend as the desktop window, behind the gateway token. |
| 15.5 | Get started onboarding | `internal/gui/assets/i18n.js` ("Get started") | auxiliary | Missing | none | First-run wizard: add a provider, connect an agent, send a test request. |
| 15.6 | UI languages: English, Simplified Chinese, Traditional Chinese, Japanese, German | `internal/gui/assets/i18n.js` | auxiliary | Missing | none | Fluent (`.ftl`) message files, English by default. Translations live only in locale files. |
| 15.7 | README in several languages | `README.md`, `README.zh-CN.md` | core | Done | `README.md` (English), `README.zh-CN.md` | Keep, with English as the default. |
| 15.8 | Start at login | `internal/autostart` | auxiliary | Missing | none | `tauri-plugin-autostart`. |
| 15.9 | Keep the computer awake while agents work | `internal/awake` | auxiliary | Missing | none | `keepawake` crate, held only while requests are in flight. |
| 15.10 | Themes and fonts; Omarchy integration | `internal/gui/assets/theme.js`, `fonts.js`; `internal/omarchy` | auxiliary | Missing | none | Low priority. Follow the system theme first. |
| 15.11 | Windows Start menu shortcut | `internal/shortcut` | auxiliary | Missing | none | Handled by the installer. |

### 16. Distribution and operations

| # | magpie feature | magpie evidence | Stage | hofund status | hofund evidence | Plan to do better than magpie |
| --- | --- | --- | --- | --- | --- | --- |
| 16.1 | Prebuilt binaries for macOS, Linux and Windows | `build/darwin`, `build/windows`; `ref.md:2322` | core | Done | `dist-workspace.toml`; `.github/workflows/release.yml`, `release-desktop.yml`; `docs/release.md` | Already better: checksums and optional signing. Add provenance attestation. |
| 16.2 | Self-update with `--proxy`, `--mirror` and auto-update | `internal/update`; `ref.md:2322` | auxiliary | Missing | none | Updater with signature checks (minisign) for the CLI and `tauri-plugin-updater` for the app. |
| 16.3 | Socket handoff on update, so the port never closes | `internal/handoff` | auxiliary | Missing | none | Pass the listening socket to the new process, then drain the old one. |
| 16.4 | Background service (systemd, launchd): `daemon install`, `status`, `restart` | `internal/daemon`; `ref.md:2382` | auxiliary | Missing | `crates/daemon` is a library, not a service installer | `hofund service install`, with unit files written atomically. |
| 16.5 | Docker image with HEALTHCHECK | `Dockerfile`; `ref.md:2455` | auxiliary | Missing | none | Distroless multi-arch image under 20 MB, running as non-root. |
| 16.6 | Termux and Android builds | `ref.md:2322` (`-tags nogui`) | auxiliary | Missing | none | Add an `aarch64-linux-android` target to cargo-dist. |
| 16.7 | Mirrors for GitHub and the model catalog | `internal/source`; `ref.md:2954` | auxiliary | Missing | none | A mirror base URL setting used by the updater and the catalog. |
| 16.8 | Agent CLI install helpers (`claude-code install`) | `internal/cli/main.go:38` | auxiliary | Missing | none | Low priority. |
| 16.9 | Daily anonymous usage ping (off with `DO_NOT_TRACK=1`) | `internal/stats`; `ref.md:3079` | — | Not planned | none | hofund sends no telemetry. That is a deliberate privacy improvement. |

## Top 10 gaps

Ranked by how much each one blocks real daily use.

1. **v0.1 protocol and wiring blockers.** The code is in open pull requests #9 and #10
   (not merged). It covers stream event order and done events (rows 1.1 to 1.3),
   field leakage across protocols (row 1.3), edits kept on disconnect (row 5.2),
   token rotation reaching the running gateway (row 10.3), `--help` and
   `examples/hofund.toml` (row 11.1), and `x-api-key` (row 10.2).
   Per-upstream model names (row 2.13) stay in stage 1 and are not a blocker.
2. **Model name per upstream (row 2.13).** Without it, failover between vendors that name a
   model differently is impossible. That is the main reason to run a router.
3. **Usage cost and cache tokens (rows 9.2, 9.3, 9.5).** These are what users look at every day.
4. **Routing strategies, stickiness and cooldown (rows 4.5 to 4.7).** Prompt-cache stickiness
   and backoff decide both cost and reliability.
5. **Provider presets, model catalog and live model lists (rows 2.7 to 2.9, 2.14).**
   Every routing, pricing and UI feature depends on these.
6. **Upstreams: Gemini, Vertex, Bedrock and Azure (rows 2.3 to 2.6).**
7. **Named gateway keys, LAN and TLS (rows 10.4, 10.5, 10.7).** `x-api-key` (row 10.2) is in PR #10.
8. **Hot reload, CLI config editing and a local management API (rows 11.2, 11.5, 12.3).**
   The UI is blocked on these.
9. **Subscriptions and quota windows (rows 3.1, 3.3, 3.8).** This is magpie's most used feature.
10. **More agent adapters, profiles and per-agent model setting (rows 5.1, 5.5, 5.6).**

## Phase plan

The execution order is [docs/roadmap.md](roadmap.md). The table below is the original
audit grouping. Later owner decisions move the terminal UI (row 15.3) and every
subscription capability, including the Claude bridge (row 3.2), into stage 1.
A phase pull request updates the rows it touches.


| Phase | Scope (rows) | PRs | Outcome |
| --- | --- | --- | --- |
| 0 | v0.1 blockers: 1.1, 1.2, 1.3, 5.2, 10.3, 11.1, and `x-api-key` from 10.2 | #9 and #10 (open) | Real agents work end to end through hofund. |
| 1 | Model name per upstream (2.13), `provider/model` and bare ids (1.20), effort mapping (1.14), `count_tokens` (1.5) | 1 to 2 | Cross-vendor failover works. |
| 2 | Ledger schema v2: cache and reasoning tokens, agent, session id, time to first byte (9.1, 9.2, 9.5, 9.9) and a richer `usage` command (9.4) | 1 | Accurate usage. |
| 3 | Model catalog (2.8, 2.14, 2.15), provider presets as data (2.7), cost with stored prices (9.3), model metadata in `/v1/models` (1.6) | 2 | Cost in USD, plus presets and catalog data. |
| 4 | Routing tree: named and nested groups (4.3), `Strategy` trait with order, weight, round-robin and least used (4.5), session stickiness (4.6), circuit breaker and `Retry-After` (4.7), several keys per provider (2.12) | 2 | Load balancing and cache-aware routing. |
| 5 | Config hot reload (11.2), `tracing` with JSON logs and request ids (12.5), `/healthz` (12.4), local management API `/hofund/v1` (12.3), routing decision log (4.13) | 2 | Base for the UI and for operating hofund. |
| 6 | Gemini, Vertex, Bedrock and Azure adapters (2.3 to 2.6); live model lists and provider test (2.9, 2.10) | 2 | Wider provider coverage. |
| 7 | Named and hashed gateway keys with scopes and limits (10.4, 9.12), LAN allow-list (10.5), auth throttling (10.6), TLS with reload (10.7), outbound proxy (10.10) | 2 | Safe sharing. |
| 8 | Desktop window over the local API: Providers, Routing, Usage, Agents pages; onboarding; Fluent i18n; autostart (15.2, 15.5, 15.6, 15.8) | 2 | Usable without editing TOML. |
| 9 | Agent adapters as data, the next 10 agents, `agent set`, profiles, visible models (5.1, 5.3, 5.5, 5.6, 5.7), and importers (2.17) | 2 | Covers most of magpie's agent list. |
| 10 | `Subscription` trait, Codex and Copilot adapters, quota model, alerts (3.1, 3.3, 3.7, 3.8, 3.10), then quota-aware strategies (4.5) | 2 | Subscriptions as providers. |
| 11 | Self-update with signatures, background service, Docker image, socket handoff (16.2 to 16.5) | 2 | Easy to run as a service. |
| 12 | Routing rules and `route explain` (4.8), context-window move (4.10), refusal re-ask (4.11), quirk schema rules and tool repair (1.17, 1.19), OTLP (12.2) | 2 | Smarter routing and better compatibility. |
| 13 | Privacy masking IR pass with a streaming restore and fail-closed modes (13.1, 13.3); request inspection ring buffer (12.1) | 2 | Privacy. |
| 14 | WebAssembly plugin runtime with the built-in middleware as rules (8.1, 8.2, 8.3); web search backends and MCP server (7.1, 7.2) | 2 | Extensibility. |
| 15 | Library (6.1 to 6.3), encrypted backup (14.1), TUI (15.3), browser UI (15.4) | 2 | Wider scope. |
| 16 | Long tail that is still auxiliary after stage 1: sync (14.2), image and vision MCP (7.3, 7.4), WSL (5.9), local PII model (13.2), and the rest. Subscriptions, including the Claude bridge (3.2), are stage 1, not this phase. | several | Full parity. |
