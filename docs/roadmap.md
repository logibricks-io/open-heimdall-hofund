# Roadmap

hofund reimplements every core capability of yetone/magpie in Rust, and does
each one better where magpie's design is unsound. The capability matrix is
[feature-parity.md](feature-parity.md). Design verdicts go in
[magpie-comparison.md](magpie-comparison.md).

Every phase pull request updates the matrix rows it touches: status, evidence,
and why the hofund behavior is better.

A pull request is based on the latest `main`. It is not stacked on an
unmerged branch. Work that depends on an unmerged pull request waits for that
pull request to merge. Independent work may proceed on its own branch.

## Stage 1 — core

Stage 1 is the product a person can run every day. Do not start stage 2 until
stage 1 has merged.

1. **v0.1 blockers.** Codec stream correctness is on `main` as #9 (`ac6325a`).
   CLI and agent behavior, including constant-time `x-api-key`, is on `main`
   as #10 (`695860d`). Per-upstream model names are not a blocker.
2. **This document set.** The matrix, this roadmap, and the parity rule in
   `AGENTS.md` and `.cursor/rules/hofund.mdc`.
3. **Model mapping.** Pull request #12. A route member names the upstream
   model (`crates/config/src/lib.rs:98`). Clients may send the route name,
   `provider/model`, or a bare wire name that exactly one hop serves.
   Reasoning effort is one IR enum mapped by each codec. `count_tokens` asks
   a Messages upstream and otherwise returns a character estimate with
   `estimated` set. A Generative Language upstream is also asked, at
   `:countTokens`. Rows 2.13, 1.20, 1.14, and 1.5 are Done in that change.
4. **Upstream coverage, including every subscription magpie supports.**
   Presets are data. Native adapters cover every non-OpenAI-compatible
   protocol magpie implements (Generative Language, Vertex, Bedrock, Azure,
   and the others in magpie's source). Live model lists and a connectivity
   test ship with them. Codex / ChatGPT, Copilot, Claude, and every other
   subscription-backed provider in magpie's source are normal providers the
   user adds in config. Credentials live in the OS keychain or in `env:`
   references, never in logs. Each adapter refreshes tokens, tracks quota,
   and returns a clear error. Provider docs include one neutral sentence that
   subscription use follows that vendor's terms. There is no hidden opt-in.
   Rows 2.3–2.10, 2.7, and 3.1–3.12.
5. **Coding agents.** Adapters are data. Every agent magpie supports is
   covered, plus `agent set` for the model and profiles. Rows 5.1–5.7.
6. **Routing.** A tree of named and nested groups, strategies, sticky
   sessions, cooldown that honors `Retry-After`, a circuit breaker, and
   several keys per provider. Rows 4.3, 4.5–4.7, 4.13, and 2.12.
7. **Usage v2 and cost.** Cache and reasoning tokens, attribution, time to
   first byte, and prices taken from the model catalog and stored on the row.
   Rows 9.1–9.5, 9.9, 9.12, 2.8, and 2.14.
8. **Operate the process, and a redesigned terminal UI.** Config hot reload,
   CLI edits that keep TOML comments, a local management API, and `GET /health`
   (already added in pull request #10). The terminal UI is core. It is a thin
   client of that management API: no business logic in the UI, no process
   globals, and no vendor branching. It is a new information architecture,
   not a port of magpie's TUI layout. The first pull request is
   `docs/tui-design.md` (study magpie's TUI, say what is unsound, ASCII
   wireframes, keymap, narrow terminals, color and no-color, accessibility).
   Implementation pull requests follow that design and snapshot-test frames
   with ratatui's `TestBackend`. Rows 11.2, 11.5, 12.3, 12.4, and 15.3.
9. **Gateway keys.** Hashed, scoped keys, and a LAN allow-list. Rows 10.4
   and 10.5.

Model mapping is pull request #12. Preset endpoints for Chat, Responses, and
Messages, including Azure's v1 root and Bedrock's API-key headers, are on
`main`. `kind = "generative"` and `preset = "google"` post `generateContent`
(row 2.3). `protocol = "chat"` on that preset keeps the OpenAI-compatible
root. Vertex credentials (row 2.4) are in. Bedrock Converse is signed with SigV4,
and Mantle chat and Responses pick `/openai/v1` or `/v1` from the model id
(row 2.5). Azure deployment names can be read from the resource
(`list_deployments`, row 2.6). That behavior is verified with mocks only,
and the parity row stays Partial until it cites a CI run. Still open in
item 4: that citation, live model lists, a connectivity test, and every
subscription provider. Agent, provider, and subscription inventories
derived from magpie's source decide the order once they are committed.
Usage v2, the catalog
and cost, and the routing tree stay in stage 1 after that. Subscriptions,
the full agent list, the management API, the terminal UI, and gateway keys
stay in stage 1 as well. One or two reviewable pull requests per phase.

## Stage 2 — auxiliary

Do not start until stage 1 has merged.

Desktop window and onboarding beyond the tray, i18n packs, self-update,
Docker and a background service, the routing-rules language and `route explain`,
OpenTelemetry, privacy masking, request inspection, plugins, web search, an
MCP server, the library, backup and sync, and the browser UI. The terminal UI
and the Claude subscription bridge are stage 1, not this list.

## Audit phases

[feature-parity.md](feature-parity.md) keeps the original P0–P16 grouping.
That grouping is not the execution sequence. P0 is the v0.1 blockers. The
terminal UI (originally P15) and every subscription (originally P10 and P16)
are stage 1, as listed above.
