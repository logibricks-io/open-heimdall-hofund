# Terminal UI

Design for hofund v0.2.0. The parity baseline is the reference terminal UI frozen for this release. This document does not use that project's name. Kernel `gateway` and `router` stay free of vendor names and of that name. Preset ids, subscription ids, and display strings come from plugin data and control responses.

The UI is a client of the control plane. It does not link `gateway`, `router`, `provider`, `codec`, or `daemon`.

## 1. Scope

Six reference pages, in order, selected by `1` through `6`, `]`, and `[` (`internal/tui/tui.go:pageKey`, `internal/tui/pages.go:pageNames`). Hofund keeps that order and those keys. Four surfaces the reference UI does not have as pages are in this release because the v0.2.0 plan asks for them: Accounts, Settings, Logs, and Help. Sessions and Library ship their screens with the shell and fill in when R-30 and R-36 exist.

Global keys (`internal/tui/tui.go:Update`, `updateList`, `View`):

| Key | Action | Work | Control |
|---|---|---|---|
| `ctrl+c` | Quit from every mode | R-11 | Handoff in section 4 |
| `q`, `esc` | Quit on a page in list mode | R-11 | Same |
| `1`–`6`, `]`, `[` | Page | R-11 | Reload that page |
| `r` | Reload the open page. On Usage, also re-read quotas | R-11 | Page GETs |
| resize | Keep the cursor row on screen | R-11 | None |
| status line | Success or failure toast, else catalog sync, else gateway line | R-11 | `GET /status`, events |

Modes (`internal/tui/tui.go` `mode` constants): list, picker, profiles, name line, typed line (`ask`), open group. `esc` returns to `back`. One mutation toast at a time. A second `d` confirms a delete; any other key clears the confirm token.

### Agents (page 1) — R-14

Reference: one row per detected agent that still has fields and a CLI (`internal/tui/tui.go:newModel`). Hidden agents are last and dimmed. Columns are the agent's own fields. Hofund agents are model, visibility, and connect state (`AgentSummary`, `AgentStatusEntry`). Effort and similar columns appear only when a later agent descriptor lists them. Do not invent those columns in this release.

| Key | Reference | Hofund | Control, already present unless noted |
|---|---|---|---|
| `j`/`k`, `up`/`down` | Next or previous agent (`updateList`) | Same | `GET /v1/hofund/agents`, `GET /v1/hofund/agents/status` |
| `h`/`l`, `left`/`right`, `tab`, `shift+tab` | Next field (`updateList`) | Cycle model, visibility, connection | `GET /v1/hofund/agents/{id}/models` |
| `enter`, space | Field picker, current value first, type-to-filter, custom value allowed (`openFieldPicker`, `picker.refilter`, `score`) | Model picker from route models. Visibility picker toggles. Connect asks for plan then connect | `POST .../model`, `POST .../visible`, `POST .../hidden_models`, `POST .../picked_only`, `POST .../plan`, `POST .../connect`, `POST .../disconnect` |
| `r` | Reload values (`updateList`) | Reload | GETs above |
| `S` | Sync the public model catalog (`syncCmd`) | Refresh listed models. No catalog-sync route exists | `POST /v1/hofund/providers/{id}/models/refresh`. Gap G-8 only if a single catalog sync is required |
| `s` | Name line, snapshot every agent (`updateName`) | Same | `POST /v1/hofund/profiles/save` |
| `p` | Profile picker (`openProfiles`) | Same | `GET /v1/hofund/profiles`, `GET /v1/hofund/profiles/{name}`, `POST /v1/hofund/profiles/switch` |
| picker `enter` | Apply (`updatePicker`) | Apply | `POST /profiles/switch` |
| picker `ctrl+e` | Rename (`updatePicker`, `updateName`) | Same | No rename route. Gap G-9 |
| picker `ctrl+d` | Delete (`updatePicker`) | Same | `DELETE /v1/hofund/profiles/{name}` |
| picker `up`/`down`, `ctrl+p`/`ctrl+n` | Move (`updatePicker`) | Same | None |
| picker `esc` | Back, reload | Same | None |
| footer | `hints` in `View` | Same keys | None |

Empty machine: the reference quits with "no supported agents found" (`tui.go:Run`). Hofund still opens, on Providers, and says no agents were detected.

### Providers (page 2) — R-14

`internal/tui/pages.go:updateProviders`, `viewProviders`, `openPresets`, `openProviderModels`.

| Key | Action | Control |
|---|---|---|
| `j`/`k` | Row | `GET /v1/hofund/providers` |
| `enter`, space | Model list. `enter` toggles a model on or off and the list stays open (`openProviderModels`) | `GET/PUT /v1/hofund/providers/{id}/picks` with `expected_picks_revision` |
| picker `ctrl+r` | Fetch that provider's models again | `POST .../models/refresh` |
| `e` | Replace the key. Password echo. Refused for a signed-in account and for a credentials-based preset | `POST .../keys` action `replace`. Gap G-1 for the row text the page shows |
| `w` | Address. Local preset, custom base URLs, or preset endpoint (`endpointAsk`, `localAddressAsk`, `customURLsAsk`) | Gap G-2 |
| `a` | Preset picker, then key, project, or address (`openPresets`, `addKeyAsk`, `vertexProjectAsk`) | Gap G-3. Remote hofund is a preset row, not a special page (`kindNote`) |
| `f` | Family tag | Gap G-2 |
| `u` | Listed in agent pickers, or groups only | Gap G-2 |
| `o` | Provider on or off (`SetOff` via `reseatCmd`) | Gap G-2. Key on/off already exists and is not the same switch |
| `t` | Connectivity test (`testCmd`) | `POST /v1/hofund/providers/{id}/test` |
| `m` | Fetch models (`refetchCmd`) | `POST .../models/refresh` |
| `b` | Balances (`balancesCmd`) | `GET /v1/hofund/quotas` (fresh). Cards with `kind=balance` |
| `d` `d` | Delete provider | Gap G-2. `reseat` of agent models is `POST .../model` after the delete |
| `r` | Reload | GET |

Row text the reference draws and `ProviderSummary` does not: display name, masked key or account label, signed-out, needs-project, switched off, family, groups-only, list error (`viewProviders`). Keys are available per id from `GET .../keys` (`masked`, `on`) but that is N+1 and still omits the other fields. G-1 extends the list response. Do not feed this page from `GET /config` text. That body can contain credentials.

### Routing (page 3) — R-17

List: `internal/tui/routing.go:updateGroups`, `viewGroups`. Open group: `updateGroup`, `viewGroup`.

| Key | Where | Action | Control |
|---|---|---|---|
| `j`/`k` | List and group | Cursor. Group cursor is members, then rules | Gap G-4 |
| `enter`, space | List | Open group | G-4 |
| `n` | List | Ask for a name, then pick the first member (`pickMsg`) | G-4 create |
| `o` | List and group | Next strategy (`nextRouting`) | G-4. Cycle: smart, order, rotate, usage, pace, weight, free, fast, cheap, drain. Names match `router` strategy ids. `manual` is reference-only. Hofund has no manual strategy. Skip it |
| `d` `d` | List | Delete group | G-4 |
| `u` | List, removed row | Restore a hidden found group | G-4 restore. Found groups that are not in hofund yet render nothing in that section |
| `esc`, `q` | Group | Back to the list. Does not quit the UI | None |
| `J`/`K`, `shift+down`/`shift+up` | Group | Swap member or rule (`move`) | G-4 order |
| `a` | Group | Add member (`openMemberPicker`, `memberOptions`) | `GET /v1/hofund/models` plus G-4 |
| `n` | Group | New rule, typed line (`openRule`, `ruleHint`) | G-4. The daemon parses the line. The TUI does not |
| `enter`, `e` | Group, on a rule | Edit that rule | G-4 |
| `d` `d` | Group | Remove member or rule. Last member cannot be removed | G-4 |
| `c` | Group | Classifier picker (`openClassifier`). Only if a rule has an intent | G-4 |
| `E` | Group, on a model member | Cycle effort (`nextMemberEffort`) | G-4 member effort |
| `F` | Group, on a model member | Toggle fast (`SetMemberFast`) | G-4 member fast |
| `s` | Group | Next affinity (`nextStays`): auto, session, turn, off, strict. Same spellings as `config::Stays` | G-4 |
| `f`, `x`, `l`, `R` | Group | Family, context tokens, reasoning levels, rename id (`agent.RenameGroup`) | G-4 |
| window | `groupWindow` | Keep the picked member or rule on screen | None |

Fallback in the reference group screen is "the first member that can take the request", plus provider `fallback` entries. Provider fallback stays on the provider (G-2 `fallback` list). Group member order is the group fallback order.

### Usage (page 4) — R-15, fed by R-10

`internal/tui/pages.go:updateUsage`, `viewUsage`, `quotaLines`, `loadQuotas`, `checkinCmd`.

| Key | Action | Control |
|---|---|---|
| `h`/`l` | Period: today, 7 days, 30 days, all (`periods`) | `GET /v1/hofund/usage?period=day\|week\|month\|all` |
| `t`, `w`, `m`, `A` | Jump to today, week, month, all | Same |
| `u` | Used percent versus left percent, persisted (`settings.QuotaLeft`) | Display uses `QuotaSpan.used` and `remaining`. Persist with G-5 `quota_left` |
| `c` | Check in every account that has a daily check-in (`checkinCmd`) | `GET /v1/hofund/checkins`, then `POST /v1/hofund/checkins/run` per row `{subscription, account}`. One toast joins the `outcome` and `detail` lines |
| `r` | Drop quotas and read again | `GET /v1/hofund/quotas` with no `kept=1` |
| open page | Asked read unless the last ask was under 30s (`quotasOpenGap`). While a read is in flight, retry every 1.5s, 20 times (`gotQuotas`) | Same GET. `standings` is not the card list. Cards are `data` |
| sparkline, agent table, model table | Built from the summary (`usageBlock`) | Bucket `UsageResponse.rows` in the client. `breakdown=agent` exists. There is no `breakdown=model`. Client groups rows by `model` |
| currency | `fmtCost` | `UsageResponse.currency` and `cny_per_usd` already |
| alerts | Not a usage key in the reference. The plan puts alerts on this page | `GET /v1/hofund/quotas/alerts`, `POST /v1/hofund/quotas/alerts/settings`. Also a `quota_alert` event toast |
| direct calls | Second block, "not through" the gateway, from session files | Not in `GET /usage`. Shown when R-30 stats exist. R-15 ships the gateway block alone |

R-10 fields (`ttft`, tokens per second, `--since`, `--group-by`) render when present on the row. Absence is an empty cell, not an error.

Check-in copy is English. The reference prints a short native label. Hofund prints `check-in` plus `outcome`.

### Sessions (page 5) — shell in R-11, page in R-44

`internal/tui/sessions.go:updateSessions`, `viewSessions`, `resumeSession`, `openSessPick`. API contract is section 5. R-44 does not wait for R-34.

| Key | Action |
|---|---|
| `j`/`k` | Session row, list view only |
| `enter` | Resume in this terminal, then restore the UI (`resumeSession`). Needs a resume command on the row |
| `h`/`l`, `t`, `w`, `m`, `A` | Range: today, 7, 30, 90, all (`sessRanges`) |
| `s`, `tab` | List versus stats chart (`sstat`) |
| `c` | Stats: tokens versus cost (`scost`) |
| `M`, `f` | Model or folder picker (`openSessPick`) |
| `x` | Clear model and folder filters |
| `r` | Reload |
| empty | If reads were never allowed, say how to allow them (`sessNothing`). Otherwise "nothing in this range" |

List columns (`sessListLines`): age, agent, folder basename, tokens, cost, title. Stats (`sessLines`, `sessChart`): totals and a daily bar.

### Library (page 6) — shell in R-11, page in R-45

`internal/tui/library.go:updateLibrary`, `viewLibrary`, `openLibAdd`, `openLibAgents`, `openServer`, `editInstructions`.

Sections, in order: instructions, MCP servers, skills, MCP servers found in agents, skills found in agents, RTK. API contract is section 5.

| Key | Action |
|---|---|
| `j`/`k` | Row, windowed so section heads stay with the cursor |
| `enter`, space | Agent toggles for instructions, MCP, skill, RTK (`openLibAgents`). Found rows explain `i` and `d` |
| `a` | Add MCP server or skills (`openLibAdd`, `probeCmd`) |
| `e` | Edit instructions in `$EDITOR` or `vi` (`editInstructions`), or retype an MCP server (`openServer`) |
| `u` | Update one skill from its source |
| `i` | Bring a found server or skill into the library. Refused when the agent owns it |
| `d` `d` | Remove from the library and from agents. Found rows say backups keep the files |
| `s` | Write the library into agents (`library.Sync`) |
| `r` | Reload |
| footer path | Directory from the view (`viewLibrary`) |

### Accounts — R-15, not a reference page

The reference shows accounts as provider rows and usage cards. It has no device-code screen. This page is hofund-only, keys `7` and also listed in Help.

| Key | Action | Control |
|---|---|---|
| `j`/`k` | Account row | G-6 list |
| `a` | Start sign-in for a subscription | G-6 start |
| dialog | Show URL or device code. `o` asks the OS to open the URL. A typed line submits a pasted final URL or a token file. `esc` cancels | G-6 poll, proof, cancel |
| `enter` | Switch the agent sign-in to this account | G-6 switch |
| `n` | Switch to the next account with room | G-6 switch target `next` |
| `i` | Import a file path | G-6 import |
| `f` | Discover. Second press with `--add` equivalent imports | G-6 discover |
| `d` `d` | Remove the account | G-6 remove |
| `x` | Refresh tokens for the row | G-6 refresh |

Sign-in never prints a token. The existing `SignInProgressEvent` (`provider`, `state`) is a hint to poll, not the challenge.

### Settings — R-18, not a reference page

| Key | Action | Control |
|---|---|---|
| `enter` | Edit the row | G-5, or the route named in the row |
| listen | Model address | G-5. Restart-only. Show `pending_restart` from `GET /status` |
| currency | `usd` or `cny` | G-5 |
| quota left | Same bit as Usage `u` | G-5 |
| discover accounts | Read agent sign-in files | G-5. `HOFUND_NO_ACCOUNTS=1` still wins for one run |
| usage alert, balance alert | Thresholds | Existing `POST /v1/hofund/quotas/alerts/settings` |
| gateway keys | Named keys and limits | Existing `GET/POST /v1/hofund/gateway-keys` |
| LAN allow-list | Display and edit the field | G-5 stores it. Enforcement is R-22. Until R-22 the row says the field is stored and not enforced |
| token rotate | Confirm, then rotate | `POST /v1/hofund/token/rotate`. Reconnect the event stream with the new token |

### Logs — R-18, not a reference page

| Key | Action | Control |
|---|---|---|
| `j`/`k` | Scroll | G-7 tail |
| `f` | Follow | G-7 |
| `e` | Toggle request lines | Existing `GET /v1/hofund/events?topics=requests,control` |
| `c` | Clear the view. Does not delete the file | None |

### Help — R-18

`?` from list mode opens the keymap for the current page. The reference has no help page. Footers stay, from `View`. `esc` closes help.

## 2. Architecture

### Crate

New package `apps/tui`, crate `hofund-tui`, binary `hofund-tui`. `hofund tui` in `apps/cli` execs that binary the same way `hofund serve` locs `hofundd`. A bare `hofund` does not open the UI. The reference opens the UI when there is no GUI (`internal/cli/main.go:run`, `runTUI`). That default is not copied.

`apps/cli` does not depend on ratatui. `hofund-tui` depends on `hofund-control-client` (proto is re-exported), `hofund-config` (config path and listen resolution only), and `hofund-secrets` (token file). Tokio is already in the graph. No dependency on `gateway`, so no axum, tower, or hyper in this crate. Update the manifest, `docs/architecture.md`, and `expected_deps` in `xtask/tests/workspace_invariants.rs` in R-11.

### Versions

ratatui and crossterm are not in the workspace and are not in the local cargo registry. R-11 pins the newest crates.io releases that `cargo deny check` accepts on the four `deny.toml` targets. Both licenses must already be in the allow list (`MIT` or `Apache-2.0`). The resolve must not add a second `mio`, `tokio`, `windows-sys`, `bitflags`, or `syn` beyond the current skip list. Record the chosen versions in `[workspace.dependencies]` in that pull request. Do not add `insta`.

### Async

One tokio runtime. The draw loop is `tokio::select` on:

- `crossterm::event::EventStream` (key, resize, focus)
- `control_client::EventStream` from `GET /v1/hofund/events?topics=control,requests`, resumed with `Last-Event-ID`
- a `oneshot` or `mpsc` for in-flight control calls

Redraw after each message. Do not draw from a worker thread. Workers only send messages.

In-flight calls use a generation counter, the same idea as `qgen` in `gotQuotas`. A stale response is dropped. Mutations are one at a time (`PendingOp { label, started }`). The status line shows the label. Overlapping reads are allowed.

### State

`App` holds `page`, `mode`, `width`, `height`, `flash: Option<Flash>`, `gateway: GatewayPresence`, `disk_revision`, and `pending`. Each page is its own struct with cursor, scroll, and last-fetch generation. Pages do not share cursors. `Flash` is `{ text, ok, until }`. A newer flash replaces the old one. It clears on the next page key.

Event application, without a refetch when the payload is enough:

| Event | Effect |
|---|---|
| `snapshot` | Replace revision, listener, in-flight count |
| `config_reloaded`, `config_rejected` | Toast, reload the open page |
| `models_refreshed` | Reload provider and picker if that provider is open |
| `agent_changed` | Reload agents |
| `sign_in_progress` | Mark the account dialog dirty and poll |
| `quota_alert` | Toast `title` and `body` |
| `token_rotated` | Drop the stream and discover again |
| `resync_required` | Reopen the stream with no cursor |
| `request_started`, `request_finished` | Logs page and Usage sparkline only |

### Layout

Full screen. Header: six page pills, matching `header`. Body: page. Blank lines pad so the status and footer sit on the last rows (`View`). Footer is one or two `hints` lines. Width is the terminal width. Lists use a window of at least 3 body rows and keep the cursor inside it (`groupWindow`, library `spend`, agents `visible := max(3, h-7)`).

### Color, contrast, Windows

Palette follows the reference adaptive colors (`tui.go` `cAccent` through `cBad`): indigo accent, muted chrome, green ok, red bad. `NO_COLOR` set and non-empty, or `--no-color`, uses the terminal default foreground and background only. Status still uses the words `ok` and `failed` and the marks `+` and `!`, so color is not the only signal. Focus is the `>` marker, not color alone.

Every action is on the keyboard. The mouse is ignored. `?` opens Help.

Windows console: crossterm enables virtual-terminal processing and raw mode, then the alternate screen. Windows Terminal and the Windows 10 console are both in scope. If raw mode fails, print one line and exit 1. Do not fall through to a broken redraw. Detach flags for the gateway child are the ones `apps/cli/src/events.rs:autostart` already uses.

## 3. Control gaps (R-7)

Present today and used as-is: the routes in `crates/kernel/gateway/src/control.rs` `router` (health, status, providers and keys, spend caps, models, refresh, test, picks, rename, catalog enable and disable, route, route session, unrouted, agents and status, plan, connect, disconnect, model, models, visible, hidden models, picked only, profiles, usage, gateway keys, quotas and history, credits, alerts, warmups, checkins, token rotate, shutdown, config get, validate, apply, reload, events). Client methods are on `crates/control-client`. Error categories are `ControlErrorKind` with the statuses in `http_status`. Bodies carry `"version": 1`. A stale `expected_revision` or `expected_picks_revision` is 409 `conflict`.

Not gaps: check-in run, quota alerts, profiles except rename, picks, provider test, usage totals, events, shutdown, config apply, daemon lock and discovery (`docs/plugins/control-plane.md`, lock `.hofund/daemon-<id>.lock`, discovery `.hofund/control-<id>.json`).

`POST /config/apply` can change providers and routes, but the TUI must not round-trip `ConfigResponse.text`. G-2 and G-4 exist so the UI never sees credential material and so a single field write can 409.

Every new JSON body has `"version": 1` and `deny_unknown_fields` on requests. Responses are `non_exhaustive`. Ids are 1 to `MAX_ID_BYTES`. Revisions are the SHA-256 disk revision already used by config apply (`validate_revision`).

### G-1 Provider list fields

Extend `ProviderSummary` additively: `name`, `enabled` (default true), `family`, `base_url`, `preset`, `account_label`, `signed_out`, `listed` (default true), `list_error`, `key_masked`. No secret. `GET /v1/hofund/providers` returns them. Omitting a new field means the old meaning.

### G-2 Provider writes

| Route | Body | Response | Errors |
|---|---|---|---|
| `POST /v1/hofund/providers` | `{version, expected_revision, preset, id?, name?, base_url?, api_key?, family?, options?}` | `{version, provider: ProviderSummary, disk_revision, runtime_revision, pending_restart}` | 422 unknown preset or bad address. 409 stale revision. 404 not used |
| `PATCH /v1/hofund/providers/{id}` | `{version, expected_revision, enabled?, family?, listed?, base_url?, fallback?}` | same envelope | 404 unknown id. 409. 422 bad fallback. `fallback` entries are `provider/model` or `group/id` |
| `DELETE /v1/hofund/providers/{id}` | `{version, expected_revision}` | `{version, disk_revision, removed: id}` | 404, 409 |

`api_key` is write-only. It is never copied into the response, events, or logs. `options` is the preset's declared table (project, location, credentials path). The TUI sends only keys the preset catalog names.

`enabled: false` removes that provider's members from routing until it is on again. `listed: false` keeps the provider valid as a group member and out of agent model pickers. Both are daemon behavior in this same change. A key's `on` flag is unchanged.

Remote hofund is `preset` of the remote-gateway plugin plus `base_url` and `api_key`. No separate route. The preset id is whatever `G-3` returns.

### G-3 Preset catalog

`GET /v1/hofund/presets` returns `{version, presets: [{id, name, kind, needs_key, needs_address, needs_project, option_keys}]}`. `kind` is `preset`, `local`, `remote`, or `custom`. Secret-free. This is the `a` picker (`openPresets`). Partners and sponsor rows are out of scope.

### G-4 Groups

Routes are `[[routes]]` groups, not a second store. `routing` is one of `smart`, `order`, `rotate`, `usage`, `pace`, `weight`, `free`, `fast`, `cheap`, `drain`. `stays` is `auto`, `session`, `turn`, `off`, `strict`.

| Route | Body | Response |
|---|---|---|
| `GET /v1/hofund/groups` | | `{version, disk_revision, groups: [Group]}` |
| `POST /v1/hofund/groups` | `{version, expected_revision, name, first_member}` | Group plus new revision. 422 if `name` is empty or the member is unknown |
| `PATCH /v1/hofund/groups/{id}` | `{version, expected_revision}` and any of `routing`, `stays`, `family`, `context`, `levels`, `classifier`, `members`, `rules`, `rename_to` | Group. 409 on stale revision. 404 unknown |
| `DELETE /v1/hofund/groups/{id}` | `{version, expected_revision}` | `{version, disk_revision, removed}` |
| `POST /v1/hofund/groups/{id}/restore` | `{version, expected_revision}` | Group. 404 if it was not a hidden found group |

`Group` is `{id, name, routing, stays, family, context, levels, classifier, members: [{ref, effort, fast, enabled}], rules: [{line}], hidden, found}`. `members` on PATCH replaces order. The client swaps locally and sends the full list. `rules[].line` is the reference rule line (`openRule`). The daemon parses and returns 422 with `FieldIssue` on `rules`. A group must keep one member. Fallback order is member order. Classifier must not be `group/{id}` itself.

### G-5 Settings

| Route | Body | Response |
|---|---|---|
| `GET /v1/hofund/settings` | | `{version, disk_revision, listen, currency, quota_left, discover_accounts, lan_allow, pending_restart}` |
| `PATCH /v1/hofund/settings` | `{version, expected_revision}` and any of those fields | Same shape, new revision |

`currency` is `usd` or `cny` (422 otherwise). `listen` is `ip:port`. A listen change is `pending_restart` and does not rebind inside the call. `lan_allow` is a list of strings stored for R-22. `discover_accounts` maps to the existing config flag. `quota_left` is the Usage `u` bit. Alert thresholds stay on the quotas routes.

### G-6 Subscriptions and accounts

The CLI paths `apps/cli/src/subscription.rs` and `apps/cli/src/accounts.rs` spawn one-shot `hofundd`. These routes call the same daemon functions (`crates/kernel/daemon/src/sign_in.rs`) inside the running daemon. A running daemon must see the new account without a restart.

| Route | Body | Response |
|---|---|---|
| `GET /v1/hofund/subscriptions` | | `{version, subscriptions: [{id, name, flow, accounts: [{id, label, ready, needs_sign_in}]}]}`. `flow` is `pkce`, `device`, `token-file`, or `bridge`. No tokens |
| `POST /v1/hofund/subscriptions/{id}/sign-in` | `{version, host?, label?, site?}` | `{version, session, flow, challenge}`. 404 unknown id. 409 if that subscription already has a session |
| `GET .../sign-in/{session}` | | `{version, state, challenge?}` `state`: `pending`, `needs_proof`, `ready`, `failed`, `cancelled` |
| `POST .../sign-in/{session}/proof` | `{version, url?}` or `{version, token?}` | Same as GET. `token` is write-only |
| `POST .../sign-in/{session}/cancel` | `{version}` | `{version, state: "cancelled"}` |
| `POST /v1/hofund/subscriptions/{id}/refresh` | `{version, account?}` | `{version, refreshed: [account id]}` |
| `POST /v1/hofund/accounts/switch` | `{version, agent, target}` | `{version, agent, account}`. `target` is `next` or an account id, email, or label. 422 if the agent has no switch |
| `POST /v1/hofund/accounts/import` | `{version, path}` | `{version, imported: [id], skipped: [{path, reason}]}`. `path` is on the daemon host. 422 if the file is not an export the plugins know |
| `GET /v1/hofund/accounts/discover` | | `{version, found: [{subscription, label, already_stored}]}` |
| `POST /v1/hofund/accounts/discover` | `{version, add: true}` | `{version, imported: [id]}` |
| `POST /v1/hofund/subscriptions/{id}/accounts/{account}/remove` | `{version}` | `{version, removed}`. This is sign-out |

`challenge` is one of `{type: "open_url", url}`, `{type: "device_code", user_code, verification_url}`, `{type: "instructions", text}`. It matches `SignInChallenge` and contains no secret. Poll at 1s while the dialog is open, and also on `sign_in_progress`. Session lifetime is 10 minutes, then `failed`. Cancel calls the plugin cancel and releases the loopback listener. 404 on an unknown session.

Extend `SignInProgressEvent` with optional `session` and `state`. Old clients ignore the extra fields.

### G-7 Log tail

`GET /v1/hofund/logs?after=<byte offset>&max_bytes=65536` returns `{version, path, offset, next_offset, lines: [string]}`. Lines are the daemon log already written by `events tail` (`.hofund/daemon-<id>.log`). The path is informational. Missing file is `{lines: [], offset: 0}`, not 404. Bytes above 64 KiB are truncated and `next_offset` says where to continue. No follow socket. The page polls at 1s while follow is on. Request lines stay on the events topic.

### G-8 Catalog sync

Only if `S` must do more than per-provider `models/refresh`. `POST /v1/hofund/catalog/sync` with `{version}` returns `{version, ok, error?}`. Otherwise R-14 does not add a route.

### G-9 Profile rename

`POST /v1/hofund/profiles/rename` `{version, from, to}` returns `ProfileListResponse`. 404 unknown. 422 empty or duplicate `to`.

### G-10 Serve role

Additive on `StatusResponse`: `serves` (`tui`, `serve`, `desktop`, or empty) and `owned_by_pid` (the process that spawned this daemon, else 0). Set from the child environment in section 4. Old clients ignore the fields.

## 4. Gateway handoff

Reference behavior:

| Step | Reference |
|---|---|
| While open | `serveGateway` (`internal/tui/gateway.go`) embeds a gateway in this process when `gateway.Running` is false |
| Probe | `ServedBy` (`internal/gateway/gateway.go:ServedBy`): GET `{URL}/` with a 700ms client. `Running` is true when the body names the product. Also reads `version`, `pid`, `window`, `serves` |
| Retry | Every 15s (`gatewayWatch`). Take the port when the other process is gone |
| Status | `gatewayStatus`: "served while the UI is open", or the bind error |
| Clean quit | `Run` stops the embedded server, then `keepGateway` (`internal/tui/keep.go`) |
| Who is handed the port | Only Linux, and only if some agent is connected (`connected`). If autostart is on, print a note and do not start anything. If a service is installed, start it. If it was never removed, install it and start it. Otherwise spawn `exe serve`, detached, log `cache/serve.log` (`serveInBackground`, `detach` in `detach_unix.go`) |
| Windows | `detach_windows.go` is a no-op. `keepGateway` returns the gone note on any OS other than Linux |
| Crash | The gateway is in-process. A kill skips `keepGateway` and the port dies with the UI |

Hofund does not embed the gateway. `hofund-tui` must not depend on `gateway`.

Protocol:

1. Probe locks with `held_daemon_locks` without creating a file or a directory.
2. If a lock is held, discover and `GET /health` and `GET /status`. Attach as a client. Do not spawn. Status uses `pid`, `model_listener.running`, and G-10 `serves`.
3. If no lock is held, `probe_model_health` for `{"status":"ok","service":"hofund"}` with a 700ms budget. If that answers, something else has the model port. Do not bind. Status says the port is taken and this config's lock is free.
4. If neither answers, spawn `hofundd` the way `apps/cli/src/events.rs:autostart` does: own process group, Windows `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, stdin closed, stdout and stderr appended to `.hofund/daemon-<id>.log`, environment plus `HOFUND_SERVES=tui` and `HOFUND_OWNER_PID`. Wait up to 10s for discovery and health. Failure is a status error, and the UI still opens.
5. Every 15s, repeat 1 through 4. Spawn only if this UI did not already spawn one and the lock is free and the model port is quiet.
6. This UI records `spawned: bool` and the child pid. It never shuts down a daemon it did not spawn.

Clean quit (`q`, `esc`, `ctrl+c`):

| Agents | Action |
|---|---|
| Any `AgentState::Connected` | Leave the child running. Print the pid and the log path on stderr. Do not call shutdown |
| None connected, and this UI spawned it | `POST /v1/hofund/shutdown`, wait until the lock is free or 10s, then exit |
| This UI did not spawn it | Exit. Leave it running |

Service install is not in this release (parity row 16.4 is missing). The reference installs a user service on the way out. Hofund prints that a future service command will do that, and leaves `hofundd` running instead. The WSL systemd sentence is a printed note, not an action.

Crash: the child is already detached, so it keeps the port. That is stricter than the reference and is what connected agents need. An orphan is visible to the next `hofund tui` as an attached daemon and stops with `hofund shutdown`. No watchdog.

Two TUIs: the second sees the lock and attaches. It must not shut the daemon down on exit.

## 5. Sessions and Library APIs

R-44 and R-45 bind only to these routes. R-30 and R-36 implement them. Paths stay stable if fields are added.

### Sessions (R-30, reads from R-29)

Consent is part of the list call so the empty state matches `sessNothing`.

`GET /v1/hofund/sessions?days=&model=&folder=&limit=`

`days` is `1`, `7`, `30`, `90`, or `0` for all. Response:

```text
{version, consent: {asked, enabled: [agent id]},
 sessions: [{id, agent, title, cwd, last_ms, models: [string],
             tokens, cost, unpriced, resume}]}
```

`resume` is an argv list plus `cwd`, or null. Null until R-34 fills it. The TUI runs it locally with the alternate screen suspended. The daemon does not spawn the agent. `enter` with null says the session cannot be resumed from here.

`GET /v1/hofund/sessions/stats?days=&model=&folder=` returns `{version, tokens, cost, unpriced, days: [{date, tokens, cost}], models: [{name, tokens}], folders: [{name, tokens}]}`. That is the chart and the `M` and `f` pickers.

`GET /v1/hofund/sessions/{id}` is the single row plus `{transcript_available}`. Transcript, markdown, export, and search are R-30 routes the page does not need for the first screen: `GET .../{id}/transcript`, `GET .../{id}/markdown`, `POST .../{id}/export`, `GET /v1/hofund/sessions/search?q=`. They must not change the list shape.

`GET/PATCH /v1/hofund/sessions/reads` is `{version, asked, enabled: [agent id]}`. The empty state points at `hofund sessions read`. The page does not toggle consent itself in R-44.

### Library (R-36)

`GET /v1/hofund/library` returns `{version, dir, error?, rows: [{kind, name, on: [agent id], about, bad, own}], agents: [{id, name, instructions, mcp, skills}]}`. `kind` is `instructions`, `mcp`, `skill`, `found-mcp`, `found-skill`, or `rtk`. That is `reloadLibrary` and `viewLibrary`.

| Route | Body | Result |
|---|---|---|
| `POST /v1/hofund/library/sync` | `{version}` | `LibraryResult` |
| `PUT /v1/hofund/library/instructions` | `{version, shared?, agents?}` | `LibraryResult` |
| `POST /v1/hofund/library/mcp` | `{version, line}` or `{version, name, replace_line}` | `LibraryResult` |
| `DELETE /v1/hofund/library/mcp/{name}` | `{version}` | `LibraryResult` |
| `POST /v1/hofund/library/mcp/{name}/agents` | `{version, agents: [id]}` | `LibraryResult` |
| `POST /v1/hofund/library/skills/probe` | `{version, source}` | `{version, skills: [{name, about}]}` |
| `POST /v1/hofund/library/skills` | `{version, source, names: [string]}` | `LibraryResult` |
| `POST /v1/hofund/library/skills/{name}/update` | `{version}` | `LibraryResult` |
| `POST /v1/hofund/library/skills/{name}/agents` | `{version, agents}` | `LibraryResult` |
| `DELETE /v1/hofund/library/skills/{name}` | `{version}` | `LibraryResult` |
| `POST /v1/hofund/library/found/{kind}/{name}/import` | `{version}` | `LibraryResult`. `kind` is `mcp` or `skill` |
| `DELETE /v1/hofund/library/found/{kind}/{name}` | `{version}` | `LibraryResult` |
| `POST /v1/hofund/library/rtk/{agent}` | `{version, on: bool}` | `{version, on: [id], restart: [id]}` |

`LibraryResult` is `{version, installed, had, changed, skipped: [{what, error}], problems: [{agent, error}]}`. The toast matches `libSaid`. Instruction text is edited locally in `$EDITOR` and sent back with `PUT`. The daemon does not open an editor.

Hooks, subagents, and projects are later library work. The row list can grow new `kind` values. The page shows an unknown kind as a dim row with no keys.

## 6. Testing

No network. Control tests use a local fake HTTP server that speaks the control JSON. The fake records method, path, and body and returns scripted envelopes, including 409 and `sign_in` sessions.

Snapshots: `ratatui::backend::TestBackend` at 100 by 30, draw one frame, compare `backend` buffer text to a file under `apps/tui/tests/fixtures/`. Diff with `assert_eq!`. No `insta`. Update fixtures only in a commit that intends a visual change. Cover each page's empty state, one selected row, a two-line footer, a toast, and a narrow height that must keep the cursor visible.

Key scenarios drive `App::handle` with crossterm key events and assert the fake's requests:

| Scenario | Assert |
|---|---|
| `2` then `j` then `t` | `POST .../test` for the second provider |
| `d` once | no DELETE. `d` again | DELETE with the revision from the last GET |
| stale 409 | toast contains the server message, cursor stays |
| `4` then `c` | one check-in POST per listed account |
| sign-in | start, poll until `device_code`, proof, cancel |
| `3` then `enter` then `J` | PATCH members in swapped order |
| `?` | help frame, `esc` restores the page |
| quit with a connected agent and `spawned` | no shutdown POST |
| quit with no connected agent and `spawned` | one shutdown POST |
| quit when attached to an existing daemon | no shutdown POST |

Windows CI runs the same TestBackend tests. One extra test calls crossterm raw-mode enter and leave. If the runner has no console, the test accepts the not-a-terminal error and fails on any other error. No interactive session and no live daemon.

Gateway handoff tests stub `held_daemon_locks`, discovery, and `probe_model_health`. They do not bind a port except one test that points the probe at a local listener returning `{"status":"ok","service":"hofund"}` and asserts no spawn.

## 7. Work order

Sizes are the plan's wall-clock sizes (S about 3–4h, M about 6–8h, L about 12–20h).

| Order | Id | Size | Deliver | Depends |
|---|---|---|---|---|
| 1 | R-7a | M | G-1, G-2, G-3, G-4, G-9, G-10. Proto, daemon, client, route tests | This document |
| 2 | R-7b | M | G-5, G-6, G-7. Sign-in must not block the daemon accept loop | R-7a |
| 3 | R-10 | M | Usage query fields the Usage page reads if present | None. Parallel with R-7 |
| 4 | R-11 | L | `apps/tui`, navigation, events, toasts, resize, handoff, empty Sessions and Library frames, deny clean | R-7a for handoff fields. Shell can use a fake before R-7b |
| 5 | R-14 | M | Agents and Providers, including preset add and remote preset | R-11, R-7a |
| 6 | R-15 | L | Usage, check-in, alerts, Accounts dialog | R-11, R-7b, R-10 |
| 7 | R-17 | M–L | Routing list and group editor | R-11, R-7a |
| 8 | R-18 | M | Settings, Logs, Help, fixtures, Windows raw-mode test | R-11, R-7b |
| 9 | R-44 | M | Sessions page on section 5 | R-11, R-30 |
| 10 | R-45 | M–L | Library page on section 5 | R-11, R-36, R-41 for skill update |

R-7 splits so provider and group routes are not blocked on sign-in. R-44 stays on the other track and uses the R-11 page trait unchanged.

### Risks

| Risk | What to do |
|---|---|
| `cargo deny` rejects ratatui's Windows or mio graph | Stay on the newest pair that passes. Do not add a skip without naming the duplicate in `deny.toml` |
| Sign-in holds a plugin task for 10 minutes | Session task is separate from the request task. Poll and cancel are cheap. UI stays live |
| Full-document config apply races a group edit | G-4 writes under the config sidecar lock and 409s on `disk_revision`. The TUI reloads and retries once |
| Detached `hofundd` outlives a crash | Intended. `hofund shutdown` is the stop. Status shows `serves=tui` |
| Linux service install is not in this release | Handoff leaves the child. Do not shell out to systemd |
| Reference `manual` routing and provider-off have no hofund strategy or flag yet | G-2 `enabled` is the off switch. Do not send `manual` |
| Sessions resume runs a local agent | Only with `resume` argv from the server. Never via a shell string on Windows. `Command` args only. The reference uses a shell (`resumeSession`). Hofund does not |
| Library `POST` can start a local MCP command | R-36 owns that policy. The TUI only sends the line the user typed |
| Page shells land before R-30 and R-36 | Empty frames call the routes and treat 404 as "not in this build yet". Adding fields must not require a new page type |
