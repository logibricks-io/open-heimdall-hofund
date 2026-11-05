# Sessions and Library design

Status: design for v0.2.0. Implementation of recording, agent-config writes, and MCP command execution waits on a second security review (section 7). This document does not use the reference project's name. Kernel `gateway` and `router` stay free of vendor names, agent ids, and that name. Agent ids, file layouts, and header names come from agent plugins.

Parity citations are `path:function` in the reference tree. Hofund enforcement sites are the modules that must contain the check. They do not exist until the row lands.

## 0. Decisions that the rest of the document depends on

| Decision | Rule |
|---|---|
| Consent default | A fresh install reads no agent session file. `asked` stays false until `hofund sessions read` or `PATCH /v1/hofund/sessions/reads`. |
| Recording default | Off. No conversation bytes are written unless recording was turned on for this machine and is still on at save time. |
| Who parses agent files | The agent plugin. `crates/kernel/sessions` and `crates/kernel/library` call plugin traits. They do not match on agent id. |
| Who serves HTTP | Only `crates/kernel/gateway`. It receives a header-name list and a hub trait from the daemon. It does not name agents. |
| Control wire | Existing `/v1/hofund/*` on the loopback control listener. Body `version` is the control proto `u32`. Mutations also send `expected_revision`. Responses add fields; they do not rename the TUI draft's paths (`r-3` design section 5). |
| Optimistic concurrency | `revision` is the lowercase hex SHA-256 of the canonical JSON of the resource. Mismatch is `409` `Conflict`. |
| Second review | Recording (R-33), library writes into agent files (R-35 and every later row that calls that writer), and MCP command execution (R-38) do not start until that review accepts this section 4. |

## 1. Sessions parity

### 1.1 Consent gate — R-29

Reference: `internal/sessions/access.go:Reads`, `SettleReads`, `SetReads`, `allowed`, `Found`; `internal/settings/settings.go:SessionReads.Reads`; `internal/cli/sessions_cli.go:sessionReadsTo`; `internal/gui/sessions_reads.go` routes.

| Behavior | Reference | Hofund |
|---|---|---|
| Fresh install reads nothing | `SettleReads` writes `{asked:false}` when no sessions cache and no `calls-v*` exist. `Reads` is false while `Asked` is false. | Same. Store is `<config>/sessions-consent.json`, mode `0600`. Missing file and missing index means read nothing. |
| Upgrade that already indexed | `readBefore` is true when `sessions.json` or `calls-v*` exists; settle sets `Asked` and `All`. | Same test against `<state>/sessions-index.json` only. Hofund has no `calls-v*`. A fresh tree has no index, so settle does not grant `All`. |
| Save failure | Settle keeps an empty `SessionReads` for this process and does not retry the write. | Same. Unwritable consent file means this process reads nothing. |
| Per-agent map | `Agents[id]` wins over `All`. Unknown id follows `All`. | Same. Ids must match `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}(@wsl:[A-Za-z0-9][A-Za-z0-9._-]{0,63})?$` (`internal/library/library.go:checkAgent`). |
| Gate on every list and on cached rows | `allowed` filters files. Readers of an already-listed session call `Reads` again. `SetReads` calls `Reset`. | `SessionIndex::list` and `SessionIndex::get` both call `Consent::allows`. Turning an agent off drops its rows before the response is built. |
| Found, not read | `Found` stats directories only. WSL distros are not opened; `All` covers them. | Plugin `SessionLocator::present` is a directory stat. It must not open session files. |
| Own-files off | `OwnOff` forces `readsNone` while gateway-only rows still show (`access.go:ownOff`). | Daemon flag `local_sessions`. Default on. Off skips every plugin read. Ledger projection still runs. |

CLI: `hofund sessions read` with no args prints asked, all, and each found agent on or off. `hofund sessions read on|off all|<agent>…` maps to `SetReads`. Unknown id is an error and writes nothing.

### 1.2 Readers — R-29, R-31, R-32

Each reader is a plugin `SessionStore`. The kernel passes the consent-filtered root and receives records. The plugin does not decide consent.

Shared parse rules, enforced in `crates/kernel/sessions` before plugin bytes are trusted:

| Limit | Value |
|---|---|
| One JSON line | 1 MiB. Longer line is skipped and the session is marked `cut`. |
| One file | 512 MiB. Parsing stops; the row is kept with `cut`. |
| SQLite | URI mode `ro`, `query_only=ON`. Fixed SQL in the plugin. No `ATTACH`. `sqlite3_limit` on length and columns. Read at most 50_000 message rows per database. |
| JSON depth | 32. |
| Path | Canonical path must stay inside the root the plugin declared for that open. See section 4.1. |

| Agent id | Work | Store | Root and files | Resume argv (no shell) | Reference |
|---|---|---|---|---|---|
| `claude` | R-29 | JSONL | `$CLAUDE_CONFIG_DIR` or `~/.claude`; `projects/*/<id>.jsonl`; subagents `<id>/subagents/agent-<id>.jsonl` | `claude` `--resume` `<id>` in `cwd` | `internal/sessions/sessions.go:ClaudeDir`, `claude.go:claudeLine`, `subagents.go` |
| `codex` | R-29 | JSONL, some zstd | `$CODEX_HOME` or `~/.codex`, plus every home `CodexHomes` finds; `sessions/**/rollout-*.jsonl` | `codex` `resume` `<id>` | `internal/appdir/codex.go:CodexHome`, `CodexHomes`; `internal/sessions/codex.go` header; `codex_resume.go:ResumeNow` |
| `opencode` | R-31 | SQLite, else JSON | `$XDG_DATA_HOME/opencode` or `~/.local/share/opencode`; `$OPENCODE_DB` or `opencode.db`. If migration `v1-v2` is completed, read `session_v2` and `session_message` only. Else `session`, `message`, `part`. No database: `storage/session`, `message`, `part` JSON | `opencode` `--session` `<id>` | `internal/sessions/opencode.go:OpenCodeDir`, `openCodeDB` |
| `pi` | R-31 | JSONL | `$PI_CODING_AGENT_DIR` or `~/.pi/agent`; `sessions/<cwd-folder>/<time>_<id>.jsonl` | `pi` `--session` `<path>` | `internal/sessions/pi.go:PiDir`, `piParse` |
| `omp` | R-31 | JSONL, same as Pi | `~/.omp/agent/sessions`, profile sessions, and `$XDG_DATA_HOME/omp/sessions` when not Windows | `omp` `--resume` `<id>` | `internal/sessions/omp.go:OmpDir`, `ompSessionRoots` |
| `gemini` | R-31 | JSON and JSONL | `$GEMINI_CLI_HOME` or `~`; `.gemini/tmp/*/chats/session-*.json` and `session-*.jsonl` | `gemini` `--resume` `<id>` when the plugin reports a binary | `internal/sessions/traces_gemini.go:geminiTraceFiles`, `geminiTmpDir` |
| Claude Desktop / Cowork | R-31 | Same JSONL as `claude`, tagged `app` | Same Claude project tree. `entrypoint` selects the desktop app id. Usage counts under `app`. Resume and transcript stay on `claude`. | none for the desktop app | `internal/sessions/sessions.go` `Session.App`; `internal/gui/sessions.go` list handler (`s.App`) |
| `cursor` | R-32 | SQLite plus protobuf blobs | Cursor config `chats/<md5(cwd)>/<chat id>/store.db` (`meta`, `blobs`). Newer `meta.json` beside it. | none when the plugin reports no CLI resume | `internal/sessions/cursor.go` header, `CursorDir` |
| `droid` | R-32 | JSONL plus settings JSON | `$FACTORY_HOME_OVERRIDE/.factory/sessions` or `~/.factory/sessions`; `<cwd>/<id>.jsonl` and `<id>.settings.json` | `droid` `--resume` `<id>` | `internal/sessions/droid.go` header |
| `grok` | R-32 | JSON plus JSONL | `$GROK_HOME` or `~/.grok`; `sessions/<url-encoded cwd>/<id>/summary.json` and `updates.jsonl` | `grok` `--resume` `<id>` | `internal/sessions/grok.go` header, `GrokDir` |
| `qoder`, `qoder-cn` | R-32 | JSONL, Claude shape | `$QODER_CONFIG_DIR` or `~/.qoder`; `$QODERCN_CONFIG_DIR` or `~/.qoder-cn`; `projects/*/<id>.jsonl` | `qodercli` `--resume` `<id>` | `internal/sessions/qoder.go:QoderDir` |
| `kimi` | R-32 | JSONL wire | `$KIMI_CODE_HOME` or `~/.kimi-code`; `sessions/<slug>/<id>/agents/<agent>/wire.jsonl` | plugin resume argv when the binary exists | `internal/sessions/calls_kimi.go:KimiCodeDir` |
| `cline` | R-32 | JSON rewritten whole | `$CLINE_SESSION_DATA_DIR`, else `$CLINE_DATA_DIR/sessions`, else `$CLINE_DIR/data/sessions`, else `~/.cline/data/sessions`; `<id>.json` and `<id>.messages.json` | `cline` `--id` `<id>` | `internal/sessions/cline.go:ClineSessionDir` |
| `hermes` | R-32 | SQLite | Hermes home and `profiles/*/state.db`. Read-only. Aggregates, not per-request rows. | plugin argv | `internal/sessions/hermes.go:HermesDir`, `HermesDirs` |
| `workbuddy`, `workbuddy-ai`, `codebuddy` | R-32 | JSONL | `$WORKBUDDY_CONFIG_DIR` or `~/.workbuddy`; `projects/**/<id>.jsonl`. CodeBuddy shares the folder when the paths are the same directory. | none (desktop) | `internal/sessions/workbuddy.go:WorkBuddyDir`, `codebuddyInWorkBuddy` |
| `zcode` | R-32 | SQLite, OpenCode tables | `~/.zcode/cli/db/db.sqlite` under the OS home, not a custom `HOME` on Windows | none | `internal/sessions/zcode.go:ZCodeDir`, `zcodeDB` |
| `dsh` | R-32 | JSONL, optional zstd frames | `$DSH_HOME/sessions` or `~/.dsh/sessions`; newest `session.jsonl`, `session.jsonl.zstd`, or `session.v<N>.*` | none until the plugin reports one | `internal/sessions/dsh.go` header |
| `agy` | R-32 | JSONL plus SQLite summary | agy data `brain/<id>/.system_generated/logs/transcript_full.jsonl`; titles in `conversation_summaries.db` | plugin argv | `internal/sessions/agy.go` header, `calls_agy.go:AgyDir` |
| `alma` | R-32 | SQLite, read-only, not deletable | OS user config `alma/chat_threads.db` | none | `internal/sessions/alma.go:AlmaDir` |
| WSL | R-32 | Same readers | On Windows, `\\wsl.localhost\<distro>\<home>\…` for distros the agent catalog already found. Cache file `<state>/sessions-wsl.json`. A stopped distro is not opened. | `wsl.exe` `-d` `<distro>` `--` plus the Linux argv | `internal/sessions/wsl.go:wslScan`, `wslList`, `wslResume` |

Subagent grouping (`internal/sessions/sessions.go` `Session.Subs`, `Copies`, `BranchOf`; `internal/sessions/subagents.go`; `codex_fork.go` header; `zcode.go` copies):

| Case | Rule |
|---|---|
| Claude sidechain / subagent file | Listed on the parent. Not a top-level row. Tokens stay on the parent when the reference counts them there. |
| Codex fork | Copied `token_count` lines that match the parent's checkpoint are not counted again. The fork's own later turns are. Parent missing: count the fork as its own file. |
| OpenCode / ZCode child | `parent_id` session is grouped under the parent. Its calls are not a second top-level session. |
| Pi fork | Lines before the fork timestamp are the parent's and are not counted twice. |
| omp subagent and advisor | Files under the session artifacts directory count in the parent. Their ids are `Subs` so gateway dedup can see them. |
| Grok `session_kind` prefix `subagent` | Counted on the parent once. |
| dsh `parentSession` | Grouped. Seed events up to `session/end-seed` (format 2) or `seedLength` (older) are not double-counted. |
| Cursor `subagentInfo` | Grouped under `rootParentAgentId` or `parentAgentId`. |

Title (`internal/sessions/sessions.go` `Session.Title`): custom title, else the agent's title, else the first user prompt, else the id. Model is the set of model ids with tokens. Folder is `cwd`. Cost uses `crates/kernel/usage` prices at the settings the index read once (`internal/sessions/sessions.go:PriceOf`). Unpriced models increment `unpriced` and add no dollars.

Daily stats (`internal/sessions/sessions.go` `state.day`, `use`; `internal/sessions/stats.go`): local date of each message. A session that crosses midnight appears on both days. Active time is gaps under the reference idle gap, on the later line's day, and is not required for v0.2.0 CLI output. Stats rows are agent, folder, model, tokens, cost.

### 1.3 Index and cache — R-29

Reference: `internal/sessions/sessions.go` package comment and `state` (`Size`, `Mod`, `Off`); `CachePath` returns `sessions.json` beside the catalog cache; `internal/sessions/revision_cache.go:trimSummaryRevisions`.

| Item | Rule |
|---|---|
| Path | `<state>/sessions-index.json`, mode `0600`. Directory `0700`. |
| Identity | Key is plugin id + canonical path + file id. |
| Reuse | Same size and mtime: do not reread. Grew: read from `Off` after the last complete line. Shrunk or mtime went backwards: parse from byte 0. |
| SQLite | Cache key is path, size, mtime, and a content revision the plugin returns (`db_revision` / `user_version`). |
| Write | Temp file in the same directory, fsync, rename over the index. Crash keeps the previous index. |
| Memory | One process-wide index. `List` returns newest-first. Default page size follows the reference list cap only when the caller asks: CLI default 20 (`sessions_cli.go:parseSessionsArgs`). Control default is every session up to 5000 (`sessions.go:All`). `Limit` 200 stays the attribution window, not the API page size. |
| Dirty bit | A consent change drops disallowed agents before save. |

### 1.4 CLI surface — R-29

Reference: `internal/cli/sessions_cli.go:parseSessionsArgs`, `sessionsTo`, `sessionList`, `sessionStats`; `internal/cli/main.go` read usage line.

| Command | Behavior |
|---|---|
| `hofund sessions` | Newest 20. Columns: age, agent, folder basename, tokens, cost, title. |
| `--days <n>\|today\|all` or `stats` | Daily totals instead of the list. Default days when the flag is bare: 7. `today` is 1. `all` and `0` are every day. |
| `--model`, `--folder` | Substring filter on model id and on `cwd`. |
| `--limit <n>` | At least 1. |
| `--json` | One JSON document, same fields as `GET /v1/hofund/sessions`. |
| `hofund sessions read …` | Section 1.1. |

`hofund sessions` does not resume, delete, or export. Those are later rows and stay off this command so a list cannot mutate.

### 1.5 Control API for sessions — R-30, R-34

Listener, bearer, and Host checks are the existing control listener. Shapes are section 5. This row is the parity map.

| Reference route | Hofund | Reference function |
|---|---|---|
| `GET /api/sessions` | `GET /v1/hofund/sessions` | `internal/gui/sessions.go:sessionRoutes` list handler |
| `GET /api/sessions/stats` | `GET /v1/hofund/sessions/stats` | same, stats handler |
| `GET /api/sessions/overview` | `GET /v1/hofund/sessions/overview` | same, overview handler (`Stats.Overview`) |
| `GET /api/sessions/progress` | `GET /v1/hofund/sessions/progress` | `sessions.Indexing` |
| `GET /api/sessions/one` | `GET /v1/hofund/sessions/{key}` | one handler |
| `GET /api/sessions/transcript` | `GET /v1/hofund/sessions/{key}/transcript` | transcript handler; `sessions.TranscriptOf`, `SubagentTranscript` |
| `GET /api/sessions/markdown` | `GET /v1/hofund/sessions/{key}/markdown` | `sessionMarkdown` |
| `POST /api/sessions/export` | `POST /v1/hofund/sessions/{key}/export` | export handler |
| `GET /api/sessions/search` | `GET /v1/hofund/sessions/search` | `internal/gui/sessions_manage.go` search handler; `sessions/search.go:SearchSession` |
| `GET /api/sessions/manage` | `GET /v1/hofund/sessions/manage` | manage handler |
| `POST /api/sessions/delete` | `POST /v1/hofund/sessions/delete` | `sessions/manage.go:Delete`; `gateway_delete.go:DeleteGateway` |
| `POST /api/sessions/restore` | `POST /v1/hofund/sessions/restore` | `manage.go:Restore` |
| `POST /api/sessions/purge` | `POST /v1/hofund/sessions/purge` | `manage.go:Purge` |
| `GET/POST /api/sessions/reads` | `GET/PATCH /v1/hofund/sessions/reads` | `sessions_reads.go` |
| `POST /api/sessions/recording` | `POST /v1/hofund/sessions/recording` | `sessions/gateway.go:SetGatewayRecording` |
| `POST /api/sessions/carry` | `POST /v1/hofund/sessions/carry` | `sessions/carry.go:carries`; `carry_convert.go` |
| `POST /api/sessions/terminal` | not a daemon route | Mac-only. The TUI runs resume locally (section 6 of the TUI draft). |
| `POST /api/sessions/relocate-claude` | `POST /v1/hofund/sessions/relocate` | `relocate_claude.go:RelocateClaudeProject` |
| `POST /api/sessions/codex-provider` | deferred | `MoveCodexProvider` changes which provider a thread lists under. Not required for the TUI. Tracked under R-34 as optional and off unless the Codex plugin implements it. |

`X-Hofund-Tag` / `X-Hofund-Same` may repeat an unchanged list when `X-Hofund-Had` matches, matching the reference list handler's "had" header. v0.2.0 may omit this optimization. The body shape stays.

### 1.6 Gateway identity, projection, dedup — R-33

Reference: `internal/gateway/gateway.go:SessionHeader`, `sessionHeaders`, `sessionOf`, `nativeSessionOf`, `conversationID`, `withGatewaySession` in `conversations.go`; `internal/sessions/external.go:MergeExternal`, `EachMerged`, `externalAdded`, `MergeExternalStats`; `internal/usage/gateway_sessions.go:GatewaySessions`; `internal/sessions/raft.go:GatewayKeys`.

Hofund ledger today (`crates/kernel/usage` `requests`) has `client_agent` and no session column. R-33 adds nullable columns, never rewritten history:

| Column | Source |
|---|---|
| `session_id` | First non-empty header in the injected list, else the opening hash. Max 128 bytes. Truncate on the byte boundary only when the cut is UTF-8, else truncate earlier. |
| `native_session_id` | First non-empty plugin header, ignoring `X-Hofund-Session`. |
| `endpoint` | Request path, for MCP rows. |
| `kind` | Empty for model calls. `mcp` for hub tool calls. |

Header policy is built by the daemon from each agent descriptor's `session_headers` (new field, same shape rules as `request_agent_header` in `docs/plugins/agents.md`). The gateway sees `Vec<HeaderName>` plus the fixed echo name `X-Hofund-Session`. Order: echo header first, then the injected list (`sessionOf`). The response sets `X-Hofund-Session` only when the request named one (`conversations.go:withGatewaySession`). The gateway never invents an id.

Opening hash, when no plugin header is present (`conversationID`): SHA-256 of the first user message bytes, or of the user messages through the first non-boilerplate one, first 12 bytes hex. Implemented in `codec` as a pure function. Gateway calls it. No agent name in that function.

Projection (`GatewaySessions`): group ledger rows by `(client_agent, session_id)` where `session_id` is non-empty. Sum tokens and `cost_usd`. Drop unidentified rows. Title is the first user text in the recording store if recording has it, else the id.

Dedup (`externalAdded`): native wins. A ledger row is hidden when any of these pairs exist on a native session: `(agent, id)`, `(agent, copy id)`, `(agent, sub id)`, `(app, id)`. Plugin id `raft` also suppresses `(pi, id)` if a raft reader exists; no raft plugin ships in v0.2.0, so that pair is unused. External rows are `read_only` and `gateway`, with no `path` and no resume. Deleted gateway sessions stay hidden until a later ledger row is newer than the trash stamp (`gateway_delete.go:GatewayHidden`).

### 1.7 Recording — R-33, second security review

Reference: `internal/gateway/conversations.go:recordConversation`, `conversationTurn`; `internal/sessions/gateway.go:SaveGatewayTurn`, `GatewayRecordingGeneration`, `PruneGatewayConversations`, `SetGatewayRecording`, `GatewayTranscript`; `internal/settings/settings.go:GatewayConversations`; `internal/redact/scrub.go:Scrub`, `ScrubJSON`.

| Limit | Value | Where |
|---|---|---|
| Default | Off | consent flag, not the library |
| Retention | 7 days (`GatewayRetention`) | prune hourly, and again on read |
| Store cap | 256 MiB (`gatewayStoreMax`) | prune oldest files after expired days |
| One session file | 256 MiB (`gatewaySessionMax`) | save refuses the append |
| One turn JSON | 1 MiB (`gatewayTurnMax`) | save error, turn dropped |
| One direction text | 64 KiB | extra parts set `cut` |
| Capture spool | 8 MiB (`conversationBodyMax`) | excess sets `cut` |
| Read back | 128 newest turns (`gatewayReadMax`) | older turns set `cut` |
| Path | `<state>/gateway-conversations/<UTC date>/<sha256(agent,id)>/turns.jsonl` | id is never a path segment |
| Mode | dir `0700`, file `0600` | section 4.2 |
| Images | stored as `[image]` | `conversationParts` |
| Tool arguments | `ScrubJSON` before they enter the turn | `conversationParts` |

Only a session id the client sent is recorded. A hashed opening id is not a recording key (`withGatewaySession` comment: an id nothing lists must not grow a file).

Generation: `GatewayRecordingGeneration` is captured when the request starts. Save no-ops unless the flag is still on and the generation matches. Clear increments the generation under the same mutex, deletes the tree, and deletes recording trash. An in-flight request cannot recreate the tree.

Prune does not hold the save lock while walking. It rechecks generation and `SameFile` before unlink. Date directories older than 7 days go first, then oldest mtime until the store is within 256 MiB.

Clear is `POST` recording `{on:false, clear:true}` or CLI `hofund sessions recording clear`. Off without clear keeps the files until prune or an explicit clear.

### 1.8 Actions — R-34

| Action | Rule | Reference |
|---|---|---|
| Resume | Control returns `resume` as `{argv, cwd}` or null. The TUI and CLI spawn `Command` with that argv. They do not join a shell string. WSL argv starts with `wsl.exe`, `-d`, distro, `--`. | `sessions.go` `Resume`; `wsl.go:wslResume`; `codex_resume.go:ResumeNow` |
| Carry, Pi to omp | No file copy. Argv is `omp` `--fork` `<pi session path>` with `cwd` set. Offered only when the omp plugin is present and the session is not read-only and not WSL. | `carry.go:carries` |
| Carry, Claude and Codex | User action only, never on list. Plugin `convert` reads the source and writes a new session in the destination agent's tree, then returns the new id and resume argv. Source bytes are not modified. | `carry.go` `Convert`; `carry_convert.go` |
| Fork | omp fork is the carry above. Codex fork is a read-side dedup, not a write. v0.2.0 does not write a Codex fork. | `codex_fork.go` header |
| Transcript | Plugin reads the listed session's own path. `sub` must be an id on that session's `subagents`. Gateway-only sessions read the recording store. | `transcript.go:TranscriptOf`; `gateway.go:GatewayTranscript` |
| Search | Case-folded substring. Cap 200 hits or 8 seconds, whichever first. Cancel with the request. Gateway-only sessions search recording text. | `search.go:SearchSession`, `SearchGateway`; `sessions_manage.go` `searchHitMax`, `searchBudget` |
| Export | Markdown of the transcript. `POST` export writes under the user's Downloads with mode `0600` and returns the path. `GET` markdown returns the bytes. | `markdown.go`; `sessions.go:sessionMarkdown` |
| Delete | Move the plugin-declared files into `<state>/trash/sessions/<key>/`. A file whose mtime changed during the move returns `active` and is left in place. Gateway-only rows move recording files and write a hide stamp. Alma and any plugin that sets `deletable: false` refuse. | `manage.go:Delete`, `TrashDir`, `ErrActive`; `gateway_delete.go:DeleteGateway` |
| Restore | Move the trash folder back to the original paths. Refuse if the destination exists. | `manage.go:Restore` |
| Purge | Delete one trash key, or all keys currently listed. Paths must stay under the trash root after canonicalize. | `manage.go:Purge` |
| Relocate | Claude plugin only. Rewrites `cwd` inside that project's JSONL and the index, with a backup folder. Body cap 16 KiB. `from` and `to` are absolute paths the plugin checks. | `relocate_claude.go:RelocateClaudeProject`; manage route body limit |

## 2. Library parity

### 2.1 Core model — R-35, second security review before any agent write

Reference: `internal/library/library.go:Library`, `Applied`, `Dir`, `path`, `load`, `save`, `change`, `changeLocked`; `backup.go:keepBackups`, `BackupDir`, `freshBackup`, `keep`, `pruneBackups`; `sync.go:Sync`; `instructions.go` markers and `SaveInstructions`.

| Piece | Path or field | Rule |
|---|---|---|
| Document | `<config>/library.json` mode `0600` | Canonical JSON, trailing newline. Sort servers, skills, projects, hooks, subagents by name. Empty `Applied` entries are omitted (`Applied.empty`). |
| Directory | `<config>/library/` mode `0700` | `instructions.md`, `instruction-sets/<id>.md`, `skills/<name>/`. |
| Lock | `flock` on `<config>/library.lock` | One writer. The lock counts acquisitions so a sync can detect that another change landed between steps (`library.go` `lock.held`). |
| Applied | `applied[agent_instance]` | Instructions hash, MCP names, skill names, tools-off lists, tool rules, hook entries as written, subagent file hashes, startup timeouts. Removal deletes only a name still in this record whose on-disk bytes still equal what was written. A user edit sticks and the record drops that name. |
| Backups | `<config>/backups/<UTC stamp>/` mode `0700`, files `0600` | First change to a path in one locked change copies the old bytes once. Keep 30 stamp directories. A directory that contains `skills/` is kept even past 30 (`pruneBackups`, `holdsSkills`). |
| Targets | plugin `LibraryTarget::discover` | Detected agents only. Missing optional files are skipped, not created, until a change must write them. |
| Sync | `Library::sync` | Load, write each target, update `Applied`, save. Failure of one agent is a `problems[]` entry and does not roll back other agents. The backup of a failed file stays. |
| Local servers | `Server.local` | Omitted from export and from inbound sync. An inbound server with the same name does not replace it (`carry.go`). |
| Skill off here | `Skill.off_here` | Not exported. Sync does not turn it back on. |

Instruction markers, hofund's own strings:

```text
<!-- hofund:begin · written by hofund from its Library -->
<!-- hofund:end -->
```

The writer replaces only the span between those lines (`instructions.go` index of `blockBegin` / `blockEnd`). A file with one marker and not the other is not rewritten; the problem is reported. Text outside the span is preserved byte for byte, including a user block that happens to mention the words. Sets: `default` is `library/instructions.md`. Other set ids are `instruction-sets/<id>.md`. `active` selects the set agents receive. An optional `file` on a set means the text lives in a user path, read on sync and written only when that set is saved.

CLI this row: `hofund library`, `hofund library sync`, `hofund library instructions`, `hofund library instructions set <file|->`, `hofund library instructions agents <a,b…|none>`.

Names: `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$` (`library.go:checkName`).

### 2.2 Control API for library — R-36

Reference routes live in `internal/gui/library.go:libraryRoutes` and `internal/gui/browser.go` browser routes. Browser MCP setup is out of v0.2.0 (no browser product in this release). Market routes land with R-41. Sign-in routes land with R-39. RTK routes land with R-43. Hooks, subagents, and projects land with R-42. Section 5 lists every path and which row implements it. R-36 implements status, sync, instructions, and the route table stubs that return `404` `NotFound` until their row lands, so the TUI can treat 404 as "not in this build".

### 2.3 MCP servers — R-37, second security review

Reference: `internal/library/mcp.go:Server`, `ServerOf`, `mcpFormat` constants, `mcpFile`; `envref.go:refsOf`; `mcp_tools_off.go`; `mcp_check.go:CheckServers`, `CheckServer`; `mcp.go` `Local`.

`Server` fields: `name`, `transport` (`stdio`, `http`, `sse`), `command`, `args`, `env`, `url`, `headers`, `agents`, `disabled_tools`, `oauth` (client id, secret, callback port), `startup_timeout` seconds, `local`, `hub` (`""` or `direct`), `note`. `note` is library-only and is never written into an agent file.

User fields: a writer loads the agent file, replaces or deletes only names in `Applied.mcp`, and copies every other key on an entry and every unknown sibling entry through (`mcp.go` foreign-entry checks used by `hubFor`).

Env refs stay `${NAME}` in `library.json`. The plugin writes the syntax that agent reads (`envref.go:refsOf`). An agent with no syntax for a field that contains a ref does not receive that server.

| Plugin format id | Header syntax | Env syntax | Reference format |
|---|---|---|---|
| claude-json | `${NAME}` | `${NAME}` | `fmtClaude` |
| codex-toml | whole-value env key, Bearer prefix allowed | env key, not an interpolated string | `fmtCodex` |
| opencode | `{env:NAME}` | `{env:NAME}` | `fmtOpenCode` |
| cursor | `${env:NAME}` | `${env:NAME}` | `fmtCursor` |
| goose | `${NAME}` plus `env_keys` | `env_keys` only | `fmtGoose` |
| dsh | `!!js` `process.env` expression | same | `fmtDsh` |
| grok-toml, hermes-yaml, pi-json, kimi, gemini, cline, zed, kiro, antigravity, alma, omp, commandcode | plugin table, default `${NAME}` when the reference `refsOf` arm says `refDollar` | same | `mcp.go` format list |

Tools-off: `disabled_tools` is written in the agent's list or deny rule. `Applied.tools_off` and `Applied.tool_rules` record the exact entries. A tool the user added to the deny list is not removed.

Local flag: CLI `hofund library mcp local <name> on|off`.

Server check (`mcp_check.go:CheckServer`): connect as a client, do not use a shell. stdio starts the same argv the writer would install, in the home directory, with `PWD` set, and with only the server's env plus a login-shell `PATH` merged by the daemon. Timeout is reported as `timeout`, not as a crash. WSL-only commands are checked inside the distro when it is running, else `wsldown`. Results: `ok`, `notfound`, `start`, `exited`, `timeout`, `http`, `refused`, `unreachable`, `wsldown`. No check output is written into the agent file.

CLI: `hofund library mcp add <name> <url|command args…> [agents=a,b…] [--local]`, `mcp agents`, `mcp local`, `mcp rm`.

Startup timeout is written only for the Codex plugin (`startup_timeout_sec`). Other plugins ignore it. A value the user set that is not the one `Applied.startup` recorded is left in place.

### 2.4 MCP router — R-38, second security review

Reference: `internal/library/mcp_hub.go:SetMCPHub`, `HubEntry`, `hubIdle`, `hubServerFor`, `ServerHub`; `internal/gateway/hub_mcp.go:HubMCPPath`, `HubMCPAll`, `hubListWait`, `hubCallWait`, `serveMCP`; `internal/library/mcp_run.go:RunPath`, `runIdle`, `specOf`; `internal/gateway/mcp.go:runMCP`; `internal/usage/probes.go:SourceMCP`, `KindMCP`.

| Item | Value |
|---|---|
| Per-agent endpoint | `POST /mcp/hofund/hub/{agent}` on the model listener |
| All-servers endpoint | `POST /mcp/hofund/hub` |
| Codex on-demand run | `POST /mcp/run/{name}` when that plugin is wired to this gateway and the server is not `direct` |
| OAuth relay | `POST /mcp/{name}` when a sign-in exists (R-39) |
| Entry name written into agents | `hofund` |
| Tool name | `<server>__<tool>`, sanitized to MCP-safe characters, max 64, numeric suffix on collision (`hubNamed`) |
| Idle, shared child | 10 minutes (`hubIdle`) |
| Idle, per Codex session child | 15 minutes (`runIdle`) |
| `tools/list` wait | 40 seconds. A server still starting is omitted and appears on a later list. |
| `tools/call` wait | 50 seconds. The error names the backend. |
| Body cap | 4 MiB (`hubBody`) |
| Backend pool | One stdio child per distinct `(command, args, expanded env)` for the hub. URL backends are HTTP clients, not processes. `direct` servers are not pooled. |
| Guard | Section 4.4. Loopback or a gateway key. Browser `Origin` refused unless it is an already-allowed control origin, which it is not on the model listener. |
| Request log | One usage row per `tools/call`: `kind=mcp`, `endpoint` the path, `client_agent`, tool name as `model` or a dedicated `tool` column, latency, status. Arguments, headers, and env are not stored. |

The hub is off until `hofund library mcp router on`. Off lists no tools and starts no process. `router <name> shared|direct` sets `hub`. A plugin that cannot speak streamable HTTP keeps per-server entries (`hubNoRemote`). If a library server or an existing user entry is already named `hofund`, the hub is not installed for that agent (`hubTaken`, `hubOwn`).

WSL agents that must see host paths for a local image-style server stay on a direct entry. v0.2.0 has no image server; the rule still applies to any plugin that sets `needs_client_roots`.

### 2.5 MCP OAuth — R-39

Reference: `internal/mcpauth/mcpauth.go` package comment, `Start`, `StartWith`, `keep`, `Cancel`, `signInTimeout`; `internal/library/mcp_signin.go:SignInServer`, `through`; `internal/gateway/mcp.go:mcpProxy`.

The reference stores access and refresh tokens in `mcp-signins.json`. Hofund does not. Tokens go to the OS credential store already used for agent keys (`docs/plugins/agents.md`, `native_credential_store`). The JSON file holds only non-secret metadata: server url, resource, client id, scope, expiry unix milliseconds, credential item url. Client secrets the user supplied are also credential-store items. Mode of the metadata file is `0600`.

Flow: MCP 2025-06-18 protected-resource metadata (RFC 9728), authorization-server metadata (RFC 8414), dynamic client registration (RFC 7591) unless the user set an OAuth app, PKCE S256. Callback listens on `127.0.0.1` only. Timeout 10 minutes. Cancel and timeout set `finished` under the flow mutex before any store write. `keep` returns an error if `finished` is set (`mcpauth.go:keep`). A late token cannot replace a previous sign-in.

Relay: agents receive `<gateway>/mcp/<name>` instead of the upstream URL when a sign-in exists and transport is `http`. SSE is not relayed. The relay adds the stored access token and drops the caller's `Authorization`. Renewal runs two minutes before expiry. Expired refresh becomes `ErrExpired` and the agents are pointed back at the upstream URL on the next sync.

CLI and control: section 5. Sign-in never returns the token to the client. Progress states: `pending`, `done`, `failed`, `canceled`.

### 2.6 Skills — R-40, R-41

Reference: `internal/library/skills.go:Skill`, `skillsDir`, `sharedSkillsDir`, `InstallSkills`, `link`, `copyIn`, `marker`, `ProbeSkills`; `skill_how.go:SetSkillHow`; `skill_handover.go:HandOverSkills`, `HandBackSkill`; `skillcheck.go:CheckSkills`; `skillnew.go:AddNewSkills`; `market.go` package comment.

| Item | Rule |
|---|---|
| Model | Directory with `SKILL.md`. Record: name, source (`github`, `git`, `local`), agents, content hash, commit, own hash (ignores dependency installs), from-repo, gone timestamp, off_here, called, note. |
| Install | GitHub archive, git clone, or a local directory. Probe lists candidate folders before install. Network is off in tests. |
| Name | `checkName`. If `SKILL.md` has another name, keep the library folder name and record `called`. |
| Link vs copy | Default link. `copy_skills` or per-agent `skill_how` of `link` or `copy`. Windows and any target the plugin marks `copy_only` always copy. A copy contains `.hofund-library` with the source path. The reference marker name is not used. |
| Shared folder | Optional. Links or copies into `~/.agents/skills` first when `shared_skills` is on (`sharedSkillsDir`). |
| Remove | `rm <name>`, `rm --all`, `rm --gone`. Only Applied links and copies. `--all` and `--gone` ask on a terminal unless `--yes`. |
| use-library / keep-own | If the agent already has that folder and it is not ours, the user chooses. use-library moves the agent's folder aside into the backup tree. keep-own drops the agent from the skill's agent list. |
| Update | GitHub commit or ETag, git remote HEAD. If the local tree hash differs from `own` and the user did not pass `--replace-edits`, refuse. |
| Hand-over | Move a skill from `~/.agents/skills` into `library/skills` and retarget links. Failure leaves the original (`skill_handover.go:HandOverSkills`). |
| Hand-back | Reverse. Agents that read the shared folder keep seeing it regardless of ticks. |
| off-here / on-here | `off_here` flag. Not part of sync export. |
| Groups | `skill_groups` in the JSON. Display-only in v0.2.0. |
| Markets | skills.sh search and the MCP registry, behind `hofund library market`. Timeouts and response size caps. No install runs until the user names the skill. Icons are not fetched in the CLI. |

R-40 and R-41 call the R-35 writer. They do not start until the write review has accepted that writer.

### 2.7 Hooks, subagents, projects — R-42

Reference: `hooks.go:Hook`, `HookEvents`, `HookEntry`; `subagents.go` file table; `projects.go:Project`.

Hooks: one command, portable event `turn-end`, `after-tool`, `before-tool`, `prompt`, `session-start`, or `<agent>:<Event>`. Matcher `shell` or `edit` maps to the agent's tool names. `Applied.hooks` stores the JSON entry exactly. Removal only if the file still contains that JSON. User hooks beside it stay. Plugins in v0.2.0 that implement hooks: Claude (`settings.json`), Codex (`hooks.json`), Gemini (`settings.json`, timeout in milliseconds), Cursor (`hooks.json`, flat entries). Others report unsupported and are skipped.

Subagents: one markdown or TOML file per agent, hash stored in `Applied.subagents`. A changed file is the user's and is not deleted. Shapes follow `subagents.go` comments for Claude, Codex, OpenCode, Gemini, Qwen, Droid, Grok, Cursor. Skills and MCP names inside a subagent are written only when that plugin says it reads them.

Projects: a directory the user adds. Skills and MCP servers are placed only in paths the plugin declares under that directory (`Placed`, `Wrote`). Empty directories hofund created are removed. A `.gitignore` is added only when the plugin asks and `Applied` records that hofund created it. Copy versus link is the project's `copy` flag.

This row uses the R-35 writer and waits on that review.

### 2.8 RTK — R-43

Reference: `internal/library/rtk.go` package comment, `rtkSpec`, `RTKURL`.

RTK is an external CLI. Hofund runs `rtk init -g` with argv the plugin lists, never a shell line. Off removes only the hook bytes or files that spec's `remove` names, and works when the `rtk` binary is gone. Install uses the plugin's installer argv (package manager or the upstream script). Upgrade follows the install channel. `path` only prepends a directory to the agent environment when the binary is missing from `PATH`. WSL: `wsl.exe -d <distro> -- rtk …` with argv. Hofund does not delete an instructions file to remove a one-line `@RTK.md` reference; it removes the line.

CLI: `hofund library rtk`, `rtk on|off <agent>`, `rtk install [distro]`, `rtk upgrade`, `rtk path`.

RTK install executes a third-party installer. It is allowed only from the CLI or control route, as the local user, and is covered by the command-execution review (section 4.4) even though the hub is a different process. The row waits on that review.

## 3. Architecture

### 3.1 Crate ownership

| Crate | Owns | Must not |
|---|---|---|
| `plugins/agents/<id>` | Locator roots, parsers, MCP/skill/hook/subagent writers, resume argv, env-ref syntax | Consent policy, HTTP, the usage ledger |
| `crates/kernel/plugin-api` | Traits `SessionStore`, `LibraryTarget`, `McpCommandSpec` | I/O policy |
| `crates/kernel/sessions` | Consent, index, merge, trash, recording files, search over records | axum, agent id literals, reading secrets |
| `crates/kernel/library` | `library.json`, lock, Applied, backups, sync orchestration, hub process table | axum, agent id literals |
| `crates/kernel/redact` | `scrub` and `scrub_json` used by recording | Logging plaintext |
| `crates/kernel/usage` | Ledger columns and MCP rows | Conversation text |
| `crates/kernel/secrets` | Credential-store trait used for OAuth tokens | Parsing session files |
| `crates/kernel/gateway` | Echo header, recording hook that calls `sessions`, MCP HTTP that calls the hub trait | Agent ids, command strings, file parsers |
| `crates/kernel/daemon` | Builds the header list and hub, serves control routes, holds the library lock owner | Format tables |
| `apps/cli` | `sessions` and `library` commands | Direct file writes into agent homes |
| `apps/tui` | Pages R-44 and R-45 | Spawning MCP servers; spawning resume except from server argv |

New edges, to be added in the same change as the crate: manifest, `docs/architecture.md` table, `xtask/tests/workspace_invariants.rs` `expected_deps`.

```text
plugin-api ← sessions ← daemon ← cli
plugin-api ← library ← daemon
redact ← sessions ← gateway
usage ← sessions
usage ← gateway
secrets ← library
```

`router` does not gain an edge. `gateway` does not depend on `library`. The daemon passes `Arc<dyn McpHub>` into the gateway constructor.

Agent plugins that are data-only `agent.toml` today grow a code module when their row needs a parser or a writer. The kernel selects them by plugin id from the registry, not by a match in kernel source.

### 3.2 Storage, permissions, locking, crash safety

| Path | Mode | Lock |
|---|---|---|
| `<config>/sessions-consent.json` | `0600` | file lock around read-modify-write |
| `<state>/sessions-index.json` | `0600` | in-process mutex plus temp rename |
| `<state>/sessions-wsl.json` | `0600` | same |
| `<state>/gateway-conversations/` | `0700` / `0600` | recording mutex |
| `<state>/trash/sessions/` | `0700` / `0600` | manage mutex |
| `<config>/library.json` | `0600` | `library.lock` flock |
| `<config>/library/` | `0700` | same flock |
| `<config>/backups/` | `0700` / `0600` | same flock |
| `<config>/mcp-signins.json` | `0600` | metadata only |

`<config>` is the directory of `user_config_path()` (`crates/kernel/config`). `<state>` is `<config>/state` so one tree can be backed up. `HOFUND_CONFIG` pointing at a file uses that file's parent.

Atomic write: create temp in the destination directory, write, fsync file, fsync directory, rename. On Windows the rename replaces the destination and does not follow a symlink as the file being replaced (section 4.3).

Crash windows:

| Crash point | Recovery |
|---|---|
| Index temp not renamed | Old index remains. Next list reparses. |
| Recording append cut mid-line | Next save truncates to the last newline (`gatewayCompleteSize`). Not `O_APPEND`, because Windows then rejects the truncate. |
| Agent file written, `library.json` not saved | Next process loads the previous Applied. Heal compares the agent file to the backup. If the file equals the intended write, heal records Applied. If it equals the backup, it retries the write. If it equals neither, it reports a problem and does not delete user bytes. |
| Consent temp not renamed | Old consent remains. A missing file still means read nothing. |

### 3.3 Windows and WSL

| Topic | Rule |
|---|---|
| Session read | Plugin returns a Windows path. Kernel canonicalizes with `\\?\` where needed. |
| Stopped distro | Do not stat through `\\wsl.localhost` if the catalog says the distro is stopped. Serve the cached listing. |
| Symlink inside the distro | The 9P mount does not follow it. The plugin may return an `open_as` path, as `WSLHome.Open` does. The kernel still checks the final path. |
| Resume | `Command::new("wsl.exe")` with args. No `cmd /c`. |
| Library file names | Instance id `codex@wsl:Ubuntu` is stored in JSON with the colon. On-disk backup folder names replace `@wsl:` with `@wsl.` (`library.go:fileName`). |
| Skill link | If the plugin says the volume cannot link, copy and write `.hofund-library`. |
| Tests | Windows CI runs path, permission, and rename tests. WSL tests are `#[ignore]` unless `HOFUND_WSL_TEST=1`. |

## 4. Security design

This section is the implementation gate. A row in section 7 that is marked review does not merge until a second review accepts the rows below and the named tests exist.

### 4.1 Sessions reading

Threat: the process reads another user's files, follows a symlink out of the agent home, parses a hostile SQLite or JSON file, or spends memory on a huge session.

| Risk | Rule | Enforcement | Test |
|---|---|---|---|
| No consent | `allows` is false until `asked` and the agent is on, or `all` is on and the agent is not explicitly off. | `sessions::consent::Consent::allows` | `consent_fresh_reads_nothing` |
| Settle cannot widen a failed save | Failed write leaves this process at read-nothing. | `sessions::consent::settle` | `consent_unwritable_reads_nothing` |
| Settle upgrade is one-way and explicit | `All` only when the index file exists before settle runs. Creating an index later must not flip consent. | `settle` writes before any index create | `consent_index_after_settle_stays_off` |
| Path traversal | Open path is the plugin root joined with a relative path that has no `..` after normalization. Canonical path must have the root as a prefix component, not a string prefix. | `sessions::open::within_root` | `open_rejects_dotdot` |
| Symlink escape | `open` uses no-follow. If the platform opens a symlink, the target is canonicalized and must still be inside the root. Refuse otherwise. | `sessions::open::open_session_file` | `open_rejects_symlink_out` |
| Symlink to a huge or special file | Same check. Refuse non-regular files. | `open_session_file` | `open_rejects_fifo` |
| Huge file | Stop at 512 MiB. Do not allocate the whole file. | `sessions::index::read_incremental` | `index_stops_at_byte_cap` |
| Huge line | Skip a line over 1 MiB. | line reader | `index_skips_long_line` |
| Hostile JSON | Depth 32. Unknown fields ignored. Strings bounded by the line cap. | `sessions::json::parse_line` | `json_depth_refused` |
| Hostile SQLite | Read-only URI, `query_only`, no `ATTACH`, no plugin-supplied SQL, row cap. | `sessions::sqlite::open_ro` | `sqlite_attach_rejected` |
| WAL side files | Opening `state.db` may create `-wal` and `-shm` even in read-only mode on some builds. Open with `immutable=1` when the file is not being written, else `mode=ro` and a test that the directory gained no new file name other than SQLite's own sidecars, which are removed if empty. | `open_ro` | `sqlite_does_not_create_user_files` |
| WSL starts a distro | Stopped distro: do not call an API that mounts it. | `sessions::wsl::listing` | `wsl_stopped_uses_cache_only` |
| Cache poisoning | Index entries whose path fails `within_root` on load are dropped. | `SessionIndex::load` | `index_drops_escaped_path` |
| Transcript path from the client | The API accepts agent and id. The path comes from the index row. | `sessions::read::transcript` | `transcript_ignores_client_path` |
| Error text | IO errors return the agent id and a generic reason. They do not include file bytes. | control mapper | `read_error_has_no_body` |

### 4.2 Conversation recording

Threat: prompts, secrets, and PII land on disk, in logs, or survive a clear.

| Risk | Rule | Enforcement | Test |
|---|---|---|---|
| Default on | Flag missing or false: `record` returns the inner writer and a no-op finish. | `gateway::record::wrap` | `recording_off_writes_nothing` |
| Invented id | Empty client session: do not record. Do not use the opening hash as a file key. | `wrap` | `recording_skips_anonymous` |
| Secret in body | `redact::scrub_json` runs on the raw input and raw output before parse, and again on each part text and name. | `sessions::record::save_turn` | `recording_scrubs_authorization_and_key` |
| PII | Scrub runs the secret class only for v0.2.0 (keys, tokens, passwords, cookies, PEM). Personal-data masking is a separate product switch and is not implied by recording. Document that prompts themselves are stored when recording is on. | `redact::scrub` | `recording_keeps_ordinary_prose` |
| Image bytes | Replaced with `[image]`. | `record::parts` | `recording_drops_image_bytes` |
| Log leakage | Save and prune log only the error category. No body, no path that contains the session id. | `record::save_turn` | `recording_log_has_no_prompt` |
| Mode | `0700` directory, `0600` file, including the temp file. | `sessions::perms` | `recording_file_mode_0600` |
| Retention | Read and prune drop turns older than 7 days. | `record::prune`, `transcript` | `recording_prunes_eighth_day` |
| Size | Store over 256 MiB deletes oldest regular files. One session file over 256 MiB refuses the append. | `prune`, `save_turn` | `recording_caps_store` |
| Clear race | Generation captured at request start must match at save. Clear bumps generation and deletes the tree under one mutex. | `record::generation` | `recording_clear_drops_inflight` |
| Re-enable race | A turn captured while off, saved after on, is dropped because the generation changed. | `save_turn` | `recording_reenable_does_not_resurrect` |
| Partial line | Truncate to the last newline before append. | `complete_size` | `recording_truncates_torn_line` |
| Path injection | Directory name is hex SHA-256 of the JSON array `[agent, id]`. | `record::identity` | `recording_path_is_hash` |
| Backup of state | A machine backup will copy these files. The product copy says recording is plaintext prompts on disk. Clear is the deletion tool. | docs in CLI help | manual review of help text |
| Control response | Transcript of a recording is allowed to the control client. It is not written to the daemon log. | control handler | `transcript_not_in_tracing` |

### 4.3 Library writes into agent files

Threat: a sync deletes user MCP entries, follows a symlink into another home, or leaves a secret in a world-readable file.

| Risk | Rule | Enforcement | Test |
|---|---|---|---|
| Delete user content | Remove an MCP name, skill link, hook entry, or subagent file only when it is listed in `Applied` and the current bytes equal the bytes hofund wrote. | `library::applied::still_ours` | `sync_keeps_user_mcp_entry` |
| Partial marker | One of the two instruction markers without the other: do not write the file. | `library::instructions::splice` | `instructions_broken_markers_untouched` |
| Marker lookalike | Only the exact line matches. | `splice` | `instructions_preserves_user_html` |
| Clobber | Write is a splice or a key replace, never a truncate-and-serialize of the whole file unless the plugin format is a map and unknown keys are copied. | each `LibraryTarget::write_*` | `writer_roundtrip_keeps_unknown_key` |
| No Applied, no delete | An agent with an empty Applied record is not stripped. | `sync` | `sync_empty_applied_deletes_nothing` |
| Atomic replace | Temp, fsync, rename. Readers never see a partial JSON document. | `library::atomic::replace` | `atomic_replace_is_complete` |
| Symlink file | `lstat` the destination. If it is a symlink, resolve one level. The final path must sit in the plugin's declared root. Else refuse. Do not unlink a symlink whose target is outside. | `library::open::prepare_dest` | `write_refuses_symlink_escape` |
| Symlink directory on the way | Each ancestor must be a real directory inside the root, or the root itself. | `prepare_dest` | `write_refuses_parent_symlink` |
| Backup before first write | `keep` runs before bytes change. Failure to backup aborts that file. | `library::backup::keep` | `failed_backup_does_not_write` |
| Backup mode | `0600`. The copy can contain the user's tokens. | `keep` | `backup_mode_0600` |
| Backup pruning | Never prune a folder that holds skill payloads. Cap the rest at 30. | `prune` | `prune_keeps_skill_backup` |
| Permissions on new agent files | Create `0600`. Do not chmod a pre-existing file to be looser. If the existing mode is wider, leave it and report `loose_mode`. | `prepare_dest` | `write_does_not_loosen_mode` |
| Windows ACL | New files get an owner-only ACL. | `perms::owner_only` | `windows_acl_owner_only` (Windows CI) |
| Env values | `${NAME}` is not expanded into the agent file when the syntax is a reference. Expansion happens only in the hub process environment. | plugin writer | `env_ref_not_expanded_in_file` |
| Secret in library.json | OAuth client secrets are not stored in `library.json`. They are credential items. | `library::save` | `library_json_has_no_client_secret` |
| Name injection | Reject names that fail `checkName`, including `..`, `/`, and `\`. | `library::name::check` | `name_rejects_path` |
| Project path | The project dir must be absolute, canonical, and not the config directory. Placed paths are relative and must stay under the project dir. | `library::project::place` | `project_place_stays_inside` |
| Crash heal | Section 3.2. Heal never deletes bytes it cannot prove it wrote. | `library::heal` | `heal_leaves_diverged_file` |
| Concurrent writers | Flock. A second process waits or returns `Unavailable` after 30 seconds. | `library::lock` | `second_writer_waits` |
| Revision | Control mutations require `expected_revision`. The lock is taken, the file is reread, and a mismatch returns `409` without writing. | daemon handler | `library_stale_revision_409` |

### 4.4 MCP router command execution

Threat: a remote caller, a web page, or a tool argument starts a process or changes which command runs.

| Risk | Rule | Enforcement | Test |
|---|---|---|---|
| Who may register a command | Only a library change from the control listener or the CLI, holding the library lock. The model listener cannot write `library.json`. | `library::change` | `model_listener_cannot_add_server` |
| Command from the request | `tools/call` arguments are not parsed as argv. The child argv is the spec stored at sync time. | `library::hub::spec_of` | `call_args_do_not_change_argv` |
| Shell | `Command::new(program)` plus an argument vector. Reject if `command` contains a newline or a NUL. Do not call `cmd.exe`, `sh`, or `bash` unless the user named that binary as the program itself. | `hub::spawn` | `spawn_does_not_use_shell` |
| PATH lookup | If `command` is not absolute, look up the basename on a fixed PATH built from the daemon's login PATH at process start. Do not use a PATH from the request. | `hub::resolve` | `spawn_ignores_request_path` |
| Env | Child env is the server `env` map after `${NAME}` expansion from the daemon environment, plus `PATH` and `PWD`. It is not the request environment. Unset references: do not start, do not write the literal `${NAME}` into a child. | `hub::env` | `unset_env_does_not_start` |
| Secrets in logs | Spawn errors name the server name, not the argv or env. | `hub::spawn` | `spawn_log_redacts_env` |
| Bind | Model listener default remains loopback (`config` listen). Hub routes exist only on that listener. | gateway router table | `hub_not_on_a_second_bind` |
| Remote caller | Non-loopback needs a valid gateway key. No key, wrong key, and query-string key are `403`. | `gateway::mcp::guard` | `hub_remote_requires_key` |
| Browser | `Origin` set, or `Sec-Fetch-Site: cross-site`, is `403`. | `guard` | `hub_rejects_origin` |
| Host header | Non-loopback Host without a gateway key is `403`, including DNS rebinding names. | `guard` | `hub_rejects_rebind_host` |
| `/mcp/run` | Stricter: loopback, empty `Origin`, loopback Host. No gateway key widens it. | `gateway::mcp::guard_run` | `run_refuses_lan_key` |
| Idle | Shared child exits after 10 minutes without a call. Per-session child after 15 minutes. Exit kills the process group (Unix) or the job object (Windows). | `hub::reap` | `idle_child_exits` |
| Process cap | At most 32 live children. The 33rd `tools/call` returns an error and does not spawn. | `hub::pool` | `pool_caps_children` |
| Output cap | Child stdout collected for one call is capped at 4 MiB. The rest is killed and reported truncated. | `hub::call` | `call_kills_huge_output` |
| Timeout | 50 seconds then kill the call's wait. The shared child stays until idle unless it is wedged (no IO for the timeout), in which case that child is reaped. | `hub::call` | `call_timeout_kills` |
| Hub off | `On() == false` spawns nothing and lists nothing. | `hub::tools` | `hub_off_spawns_nothing` |
| Direct flag | `hub=direct` is not spawned by the hub. | `in_hub` | `direct_server_not_pooled` |
| Request log | Row has no arguments and no env. | `usage` insert for kind `mcp` | `mcp_row_has_no_args` |
| RTK and check | `rtk init` and server check use this same spawn helper. | `hub::spawn` | `check_uses_spawn_helper` |

The local user can still register any command. That is the product. The design closes remote and cross-site registration and argument injection, not the user's own argv.

### 4.5 MCP OAuth tokens

| Risk | Rule | Enforcement | Test |
|---|---|---|---|
| Token in JSON | Access token, refresh token, and client secret are credential-store items. `mcp-signins.json` fails its own load if those keys appear. | `library::oauth::save_meta` | `signins_file_rejects_token_field` |
| Keychain account | Item url is `mcp-oauth:<name>` and never contains the token. Store errors do not include the secret. | `secrets` item builder | `oauth_item_url_has_no_token` |
| Late token | `finished` is set under the flow mutex by cancel, timeout, and a previous success. `keep` writes only when `finished` is false, and sets it before releasing the mutex. | `library::oauth::Flow::keep` | `late_token_does_not_save` |
| Callback bind | `127.0.0.1` and the configured port, or an ephemeral port when the app says any. Refuse `0.0.0.0`. | `oauth::listen` | `callback_is_loopback` |
| Open redirect | Redirect URI is the callback hofund opened, not a URI from the query string. | `oauth::finish` | `redirect_uri_is_ours` |
| State | Random 32 bytes, compared in constant time, single use. | `Flow` | `state_reused_rejected` |
| Timeout | 10 minutes moves the flow to `failed` and sets `finished` before the HTTP client returns. | `Flow::watch` | `timeout_blocks_late_keep` |
| Relay auth | Upstream `Authorization` is the stored token. Caller `Authorization` is a gateway key check, then stripped. | `gateway::mcp::proxy` | `relay_strips_caller_authorization` |
| Page or LAN | Same guard as section 4.4. A stolen gateway key can use the relay while the listener is shared. v0.2.0 does not widen the model listener. Document that sharing the listener shares the relay. | `guard` | `relay_closed_when_loopback` |
| Sign-out | Deletes the credential item and the metadata row, then syncs agents back to the upstream URL. | `oauth::sign_out` | `sign_out_removes_item` |

### 4.6 Residual risks the design does not pretend to close

| Risk | Why it remains | What v0.2.0 does |
|---|---|---|
| Recording stores conversation text | That is the feature, after opt-in | Scrub secrets, cap size and time, mode `0600`, clear |
| User-authored MCP command is arbitrary code | The user typed it | Only that user, only via control or CLI, never from a tool call |
| Market and git install talk to the network | Install is the feature | Tests use a local fake. Production timeouts and size caps. No install from a search result without a name the user sent |
| Agent files that were already world-readable stay that way | chmod would surprise the user | Do not loosen. Report `loose_mode` |
| A plugin bug can still mis-parse | Parsers are new code | Kernel bounds and the consent gate limit the blast radius. Fuzz fixtures in section 6 |
| Ledger projection shows titles of traffic the user did not consent to read from disk | The ledger is hofund's own log of requests it proxied | Projection does not open agent files. Recording text still requires the recording flag |

## 5. Control API and CLI

All routes are under the existing control listener. `version` is the control proto version. Mutating JSON bodies include `expected_revision`. Unknown request fields are rejected. Error body is `ErrorEnvelope`. Kinds used here: `Unauthorized` 401, `Forbidden` 403, `NotFound` 404, `Conflict` 409, `BadRequest` 400, `Unprocessable` 422, `Unavailable` 503, `Timeout` 504, `Internal` 500.

Session key in paths is `agent:id` with the colon percent-encoded. The JSON still has `agent` and `id`.

### 5.1 Sessions routes

| Route | Request | Response | Revision and errors |
|---|---|---|---|
| `GET /v1/hofund/sessions` | Query `days`, `model`, `folder`, `limit`. `days` is `1`, `7`, `30`, `90`, or `0` for all. Omitted `days` does not filter. | `{version, revision, consent:{asked, all, enabled:[]}, sessions:[{key, id, agent, title, cwd, last_ms, models:[string], tokens:{input, output, cache_read, cache_write}, cost, unpriced, resume:{argv, cwd}\|null, read_only, gateway}]}` | `400` on a bad query |
| `GET /v1/hofund/sessions/stats` | `days`, `model`, `folder` | `{version, revision, tokens, cost, unpriced, days:[{date, tokens, cost}], models:[{name, tokens}], folders:[{name, tokens}]}` | `400` |
| `GET /v1/hofund/sessions/overview` | `days`, `agent`, `model`, `cwd` | `{version, count, median_cost, by_day:[{date, count, cost}], top:[{key, title, cost}]}` | `400` |
| `GET /v1/hofund/sessions/progress` | none | `{version, files_total, files_done, running}` | none |
| `GET /v1/hofund/sessions/{key}` | none | One session object plus `transcript_available` | `404` |
| `GET /v1/hofund/sessions/{key}/transcript` | Query `sub` optional | `{version, parts:[{role, kind, name, text}], cut, source}` | `404`, `403` if consent is off |
| `GET /v1/hofund/sessions/{key}/markdown` | none | `text/markdown` body | `404` |
| `POST /v1/hofund/sessions/{key}/export` | `{version}` | `{version, path}` | `404` |
| `GET /v1/hofund/sessions/search` | `agent`, `q` | `{version, hits:[{key, id, excerpt}], cut}` | `400` empty `q` |
| `GET /v1/hofund/sessions/manage` | Query `agent` | `{version, revision, agents:[{agent, count, deletable}], agent, sessions:[…], trash:[{key, agent, id, title}], trash_dir, recording, recorded}` | none |
| `GET /v1/hofund/sessions/reads` | none | `{version, revision, asked, all, found:[{agent, on}]}` | none |
| `PATCH /v1/hofund/sessions/reads` | `{version, expected_revision, all?:bool, agents?:{id:bool}}` | the GET body | `409`, `400` bad id |
| `POST /v1/hofund/sessions/recording` | `{version, expected_revision, on:bool, clear:bool}` | `{version, revision, recording:bool, recorded:bool}` | `409`. Review gate |
| `POST /v1/hofund/sessions/delete` | `{version, agent, ids:[string]}` | `{version, deleted:[id], refused:[{id, error, active:bool}]}` | `400` |
| `POST /v1/hofund/sessions/restore` | `{version, key}` | `{version, agent, id, title}` | `404`, `409` if destination exists (`Conflict`) |
| `POST /v1/hofund/sessions/purge` | `{version, keys?:[], all?:bool}` | `{version, purged:[key], refused:[{key, error}]}` | `400` if both empty |
| `POST /v1/hofund/sessions/carry` | `{version, key, to}` | `{version, agent, id, command:{argv, cwd}}` | `404`, `422` if the pair cannot convert |
| `POST /v1/hofund/sessions/relocate` | `{version, agent, from, to}` | `{version, moved, backup}` | `422`, plugin refuses unknown agent |

`resume` is null until R-34 fills it. R-44 treats null as "cannot resume from here".

### 5.2 Library routes

`GET /v1/hofund/library` returns `{version, revision, dir, rows:[{kind, name, on:[agent], about, bad, own}], agents:[{id, name, instructions, mcp, skills}]}`. `kind` is `instructions`, `mcp`, `skill`, `found-mcp`, `found-skill`, `rtk`, and later `hook`, `subagent`, `project`. Unknown kind is a dim row in the TUI.

| Route | Body | Result | Row |
|---|---|---|---|
| `POST /v1/hofund/library/sync` | `{version, expected_revision}` | `LibraryResult` | R-36 |
| `PUT /v1/hofund/library/instructions` | `{version, expected_revision, shared?, agents?, set?}` | `LibraryResult` | R-36 |
| `POST /v1/hofund/library/mcp` | `{version, expected_revision, line}` or `{version, expected_revision, name, replace_line}` | `LibraryResult` | R-37 |
| `DELETE /v1/hofund/library/mcp/{name}` | `{version, expected_revision}` | `LibraryResult` | R-37 |
| `POST /v1/hofund/library/mcp/{name}/agents` | `{version, expected_revision, agents:[]}` | `LibraryResult` | R-37 |
| `POST /v1/hofund/library/mcp/{name}/local` | `{version, expected_revision, on:bool}` | `LibraryResult` | R-37 |
| `POST /v1/hofund/library/mcp/check` | `{version, names:[]}` | `{version, health:[{name, why, detail}]}` | R-37 |
| `POST /v1/hofund/library/mcp/router` | `{version, expected_revision, on:bool}` | `LibraryResult` | R-38 |
| `POST /v1/hofund/library/mcp/{name}/route` | `{version, expected_revision, mode:shared\|direct}` | `LibraryResult` | R-38 |
| `POST /v1/hofund/library/skills/probe` | `{version, source}` | `{version, skills:[{name, about}]}` | R-40 |
| `POST /v1/hofund/library/skills` | `{version, expected_revision, source, names:[]}` | `LibraryResult` | R-40 |
| `POST /v1/hofund/library/skills/{name}/update` | `{version, expected_revision, replace_edits?:bool}` | `LibraryResult` | R-41 |
| `POST /v1/hofund/library/skills/{name}/agents` | `{version, expected_revision, agents}` | `LibraryResult` | R-40 |
| `DELETE /v1/hofund/library/skills/{name}` | `{version, expected_revision, gone?:bool, all?:bool}` | `LibraryResult` | R-40 |
| `POST /v1/hofund/library/skills/{name}/how` | `{version, expected_revision, how:link\|copy, agent?}` | `LibraryResult` | R-40 |
| `POST /v1/hofund/library/skills/{name}/here` | `{version, expected_revision, on:bool}` | `LibraryResult` | R-41 |
| `POST /v1/hofund/library/skills/{name}/hand` | `{version, expected_revision, direction:over\|back}` | `LibraryResult` | R-41 |
| `POST /v1/hofund/library/found/{kind}/{name}/import` | `{version, expected_revision}` | `LibraryResult`. `kind` is `mcp` or `skill` | R-40 |
| `DELETE /v1/hofund/library/found/{kind}/{name}` | `{version, expected_revision}` | `LibraryResult` | R-40 |
| `GET /v1/hofund/library/market/skills` | Query `q` | `{version, skills:[{name, about, source}]}` | R-41 |
| `GET /v1/hofund/library/market/servers` | Query `q` | `{version, servers:[{name, about, transport}]}` | R-41 |
| `POST /v1/hofund/library/mcp-signin` | `{version, name}` | `{version, id, state, url}` and no token | R-39 |
| `GET /v1/hofund/library/mcp-signin/{id}` | none | `{version, state, error?}` | R-39 |
| `POST /v1/hofund/library/mcp-signin/{id}/cancel` | `{version}` | `{version, state:canceled}` | R-39 |
| `POST /v1/hofund/library/mcp-signout` | `{version, expected_revision, name}` | `LibraryResult` | R-39 |
| `POST /v1/hofund/library/rtk/{agent}` | `{version, expected_revision, on:bool}` | `{version, revision, on:[], restart:[]}` | R-43 |
| `POST /v1/hofund/library/hooks` | `{version, expected_revision, hook}` | `LibraryResult` | R-42 |
| `DELETE /v1/hofund/library/hooks/{name}` | `{version, expected_revision}` | `LibraryResult` | R-42 |
| `POST /v1/hofund/library/subagents` | `{version, expected_revision, subagent}` | `LibraryResult` | R-42 |
| `DELETE /v1/hofund/library/subagents/{name}` | `{version, expected_revision}` | `LibraryResult` | R-42 |
| `POST /v1/hofund/library/projects` | `{version, expected_revision, dir, copy?:bool}` | `LibraryResult` | R-42 |
| `DELETE /v1/hofund/library/projects` | `{version, expected_revision, dir}` | `LibraryResult` | R-42 |
| `GET /v1/hofund/library/backups` | none | `{version, backups:[{stamp, files}]}` | R-36 |

`LibraryResult` is `{version, revision, installed, had, changed, skipped:[{what, error}], problems:[{agent, error}]}`.

`line` for MCP add is a URL or an argv string split with POSIX quoting rules in the daemon, not a shell. The TUI sends the line the user typed. The daemon rejects a line that quotes a shell metacharacter sequence it cannot split (`400`).

### 5.3 CLI parity

| Reference command | Hofund | Row |
|---|---|---|
| `sessions` with `--days`, `--model`, `--folder`, `--limit`, `--json` | `hofund sessions` with the same flags | R-29 |
| `sessions read [on\|off all\|<agent>…]` | `hofund sessions read …` | R-29 |
| `sessions recording on\|off\|clear` | `hofund sessions recording …` | R-33 |
| `library` | `hofund library` | R-35 |
| `library sync` | `hofund library sync` | R-35 |
| `library instructions [set\|agents]` | same under `hofund` | R-35 |
| `library mcp add\|agents\|local\|rm` | same | R-37 |
| `library mcp router on\|off` and `<name> shared\|direct` | same | R-38 |
| `library skill …` including `how`, `hand-over`, `hand-back`, `off-here`, `on-here`, `update`, `rm --all`, `rm --gone`, `use-library`, `keep-own` | same | R-40, R-41 |
| `library rtk …` | same | R-43 |
| backup tool `--no-library` | `hofund` state backup excludes `library.json`, `library/`, and `backups/` when `--no-library` is set | R-35 |

There is no CLI for delete, carry, or relocate in v0.2.0. Those are control routes so a script can use them, and the TUI uses resume only.

## 6. Testing

No network in `cargo test`. GitHub, skills.sh, the MCP registry, and OAuth token endpoints are `httptest` listeners or traits. Child processes in hub tests are a fixture binary in the repo that speaks one JSON-RPC response and exits.

| Suite | What it proves |
|---|---|
| `plugins/agents/<id>/tests/fixtures` | One directory per format listed in sections 1.2 and 2.3. Golden JSON of the parsed session or of the written agent file. Diff with `assert_eq` on normalized JSON. |
| Consent | Section 4.1 tests, including a fixture index that must not grant consent by itself after settle has written `asked:false`. |
| Recording | Temp dir, assert mode, scrub, generation, and that tracing subscriber events contain no fixture secret. |
| Library writer | Start from a fixture agent file that contains a user entry and a hofund entry. Sync remove. Assert the user entry remains and the hofund entry is gone. |
| Symlink | Fixture with a symlink pointing outside the temp root. Assert the bytes outside are unchanged. |
| Hub | Fake stdio server. Assert argv equals the library spec when the tool call contains `"; rm"`. Assert idle reap. Assert `Origin` gets 403. |
| OAuth | Fake authorization server. Cancel, then deliver the code. Assert the credential fake is empty. |
| Windows CI | `perms`, atomic rename over an existing file, no-follow open, and `Command` argv for resume. No live WSL. |
| Fuzz | One fuzz target on the JSONL line parser and one on the instruction splicer, corpus checked in, run for a bounded time in CI. |
| TUI | Existing TestBackend plan. Sessions and Library frames against the section 5 shapes. `enter` with null resume does not spawn. `enter` with argv spawns a fake `Command` factory. |

Golden files live next to the plugin, not in the kernel. The kernel tests use a `FakeStore` that returns fixed records.

## 7. Work breakdown

Sizes match the backlog: S about 3–4h, M about 6–8h, L about 12–20h. Review means a second security review of section 4 must accept the change before implementation of that row starts. Design of the row can proceed.

| Order | Id | Size | Delivers | Depends | Review before code |
|---|---|---|---|---|---|
| 1 | R-29 | L | Consent, index, Claude and Codex readers, daily stats, `hofund sessions` and `sessions read` | This document | No. Readers are bounded and consent-gated. No recording and no agent writes. |
| 2 | R-30 | M | Session control routes except recording, carry, delete, relocate | R-29 | No |
| 3 | R-31 | L | OpenCode, Pi, omp, Gemini, Claude Desktop tag | R-29 | No |
| 4 | R-32 | L | Remaining readers and WSL cache | R-29 | No. WSL must not start a stopped distro; that test is part of the row. |
| 5 | R-33 projection | part of L | Ledger columns, header injection, merge and dedup | R-29 | No |
| 6 | R-33 recording | part of L | Opt-in store, scrub, prune, clear | R-29 and the review | Yes |
| 7 | R-34 | M–L | Resume argv, carry, transcript, search, export, delete, restore, purge, relocate | R-30, R-31 | No, provided delete uses the trash root checks in section 4.1. Relocate is a Claude plugin write and uses `prepare_dest`. |
| 8 | R-35 | L | library.json, lock, Applied, backups, instruction splice, sync engine, library CLI status and instructions | This document and the review | Yes. This is the writer every later library row calls. |
| 9 | R-36 | M | Library control routes for status, sync, instructions, backups. Later routes return 404 until their row. | R-35 | The sync route is covered by the R-35 review. Do not implement sync before that review. |
| 10 | R-37 | L | MCP formats, env refs, tools-off, local, check, MCP CLI | R-35 | Yes. It writes agent config and can spawn a check process. The spawn waits on the R-38 review as well if the check lands in the same change as the writer. Split the writer from the spawn if needed so the writer review is not blocked on the process review. |
| 11 | R-38 | L | Hub endpoints, pool, idle,| 11 | R-38 | L | Hub routes `/mcp/hofund/hub` and `/mcp/hofund/hub/{agent}`, `/mcp/run/{name}`, backend pool, 10 minute and 15 minute idle, loopback guard, 50 second call wait, usage rows with `kind=mcp` and no arguments | R-37 | Yes. Command execution (section 4.4). |
| 12 | R-39 | M | OAuth sign-in, credential-store tokens, metadata file, cancel and timeout, relay `/mcp/{name}` | R-37 | Yes for section 4.5. No child process, but a late token must fail the named test before merge. |
| 13 | R-40 | L | `SKILL.md` model, GitHub, git, and local install, link versus copy, `~/.agents/skills` first, `rm`, use-library, keep-own | R-35 | Yes. Uses the R-35 writer. Do not start until that review has accepted the writer. |
| 14 | R-41 | L | Update with commit or ETag, refuse local edits unless `--replace-edits`, hand-over, hand-back, off-here, groups, skills.sh and MCP registry | R-40 | Yes. Same writer. Market clients are fakes in tests. |
| 15 | R-42 | M–L | Hooks, subagents, project skill and MCP folders | R-37, R-40 | Yes. Same writer. |
| 16 | R-43 | M | RTK on and off per agent, install, upgrade, path, WSL argv | R-35 | Yes. `rtk init` goes through `hub::spawn` and waits on the section 4.4 review as well as the writer review. |
| 17 | R-44 | M | TUI Sessions page: range, model, folder, newest first, enter resumes from server argv, `s` shows totals and the day chart | R-11, R-30 | No. Resume with a null argv does not spawn. Resume with argv uses `Command` and does not wait on R-34 for the empty state. |
| 18 | R-45 | M–L | TUI Library page: instructions, MCP and router rows, skills, per-agent ticks, found rows, RTK | R-11, R-36, R-41 | No extra review. Mutations call routes whose implementations already waited. A 404 is the empty "not in this build" frame. |

Rows that write agent files (R-35, R-37, R-40, R-41, R-42, R-43) share one writer. The second review is of that writer plus section 4.3, not a separate review per row, unless a row adds a new write primitive. R-38 and the RTK installer share one spawn helper. The second review is of section 4.4 once. Recording is a separate review of section 4.2 and is not bundled with the writer.

### Risks

| Risk | What the row does |
|---|---|
| Reader count is larger than two plugins | R-29 lands the kernel bounds and two readers. R-31 and R-32 add parsers behind the same `within_root` and consent checks. A parser bug cannot widen consent. |
| Ledger has no session column today | R-33 adds nullable columns. Old rows stay unidentified and are not projected. |
| Header names would leak into `gateway` if listed there | The daemon builds the list from plugin descriptors. Gateway tests use a fixed list with no agent id in source. |
| Sync deletes a user edit that merely looks similar | `still_ours` requires byte equality with the Applied record. A mismatch sticks and is reported. |
| Windows rename and ACL differ | R-35 includes the Windows CI tests in section 4.3 before any other writer row merges. |
| Hub and check both spawn | One `spawn` helper. R-37 check does not ship a second process starter. |
| OAuth file from the reference would store tokens | R-39 refuses a metadata file that contains token fields. Tests use the in-memory credential store. |
| TUI resume via a shell string | R-44 accepts only `{argv, cwd}` from the server. A string field is ignored. |
| Page ships before the routes | R-11 empty frames treat 404 as unavailable. R-44 and R-45 do not fork the page type when fields are added. |

### Exit checks for the whole set

| Check | Pass condition |
|---|---|
| Consent | A fresh config directory lists no session body from disk until `sessions read on`. |
| Recording | With the flag off, the conversations directory is absent after a proxied request that contains a fixture secret. |
| Applied | A fixture file that mixes a user entry and a library entry loses only the library entry on `rm`. |
| Spawn | A hub `tools/call` whose arguments contain shell metacharacters leaves the child argv equal to the stored spec. |
| OAuth | Cancel, then a token response, leaves the credential store empty. |
| Kernel source | `crates/kernel/gateway` and `crates/kernel/router` contain no agent id and no reference-project name. Plugin crates own the format tables. |
| Docs | The architecture dependency table and `expected_deps` list the new edges in the same change as the crate. |