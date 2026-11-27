# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- D-279: Claude's `allowed-ways-out` list of region codes, addresses, and CIDR ranges, when set, alone judges the way out. An empty list still uses the supported-region list. A stored reading is judged again on every read. With `region-hold` on, a reading of any state older than 30 seconds is looked up again first, within 5 seconds, and one lookup is shared. The trace keeps the address beside the region, and the provider row and the menu show that address, the region, and Check. Verified with an injected clock and a loopback trace only.
- D-259: a dropped Claude bridge request shelves a session cut at a known end (the saved session the run started from, or the preceding assistant row whose stop reason is `end_turn`) and lets the run go. A different next turn resumes that cut and is told only that turn. A request for the same turn waits without missing a completed cut, and an in-flight client disconnect records 499. Cancellation uses the request after advisor and stage-D mutations. Pre-drain shutdown and agent sign-in account handover suppress late cuts. A new run, a request carrying client cache marks, and a run that does not persist a session keep nothing; retiring, replacing, expiring, or trimming a shelf entry deletes its bridge-written copy.

### Fixed

- D-279: Cancelling the lookup a held request leads removes that shared lookup, so a later hold is not stuck and starts a new lookup. A waiter leaves after 5 seconds with the stored reading, and that 5 seconds is the whole lookup: when it elapses the reading is stored as unknown with a generated message. The desktop usage window is allowed to call Check. Verified with an injected clock and a loopback trace only.
- D-277: Provider tests and gateway requests now reject a web page, an empty 200, or JSON-labelled non-JSON with a 502 describing the reply, including a page title when one is present, instead of reporting success. An unversioned Chat or Responses relay base that returns its web page is retried once under `/v1`, with the corrected base reused immediately and persisted across restarts; a refusal under `/v1` keeps its status and includes the base-URL hint, and a 404 or another page there keeps the original error. Verified with loopback mocks only.
- D-280: a Chat or Generative Language stream that names its prompt on more than one chunk, and the cache on only some of them, keeps the last positive prompt, cache read, and cache write. A later zero no longer drops the prompt while the cache remains, so a Responses client is told the prompt once beside the cache and the ledger records that prompt with the cache read once. Messages and Responses upstreams are unchanged. Verified with deterministic stream fixtures only.
- D-276: Live model rows marked `"type": "decision"` are kept off the default chat model list, including rows with a separate chat `kind`. OpenCode Go's preset now declares its System One `Decide` base as data only; no decider runtime is added. Verified with loopback mocks only.
- D-270: Generative Language providers now send `thinkingLevel: xhigh` only when the model's advertised levels include it; generation 3, models without that level or with unknown levels, and `max` remain `high`, and same-protocol `thinkingConfig` objects stay unchanged. Verified with deterministic provider and codec tests only.
- D-269: Zhipu and Z.ai 429 responses for Chinese 1308/1310 window exhaustion now rest as quota even when typed `rate_limit_error`, whether they include the reset sentence or only the usage-limit wording, so the existing renewed-window handling applies. Dated reset sentences with UTC/GMT or numeric offsets, including bracketed offsets, are recognized; cut or unterminated offsets are not. The 1302 per-minute response and other per-minute ceilings remain rate limits. Verified with fixtures only.
- D-272: a Codex subscription 400 saying `Unsupported input item: agent_message` now retries with every readable `agent_message` rewritten as a user turn, including its `From <author> to <recipient>` header (or the available one-sided header) and original text and images. Codex shaping lifts the codec-wrapped item back to top level with its id, so recovery acts on the body actually posted. Sealed, empty, and other shapes stay unchanged. Foreign-seal retries remove a named `amsg_...` item, or replace sealed `MESSAGE` parts with a neutral explanation when that id is absent, preserving `NEW_TASK`. Kernel HTTP Responses members record only seal hashes actually removed by the retry, per conversation and member. Their prepare path handles both top-level and codec-wrapped agent messages, so later encoded requests leave out old `MESSAGE` seals while keeping `NEW_TASK` and new seals. Codex subscription accounts do not yet have cross-turn seal memory, so later turns pay another 400 plus retry. Verified with real Responses codec decoding and encoding, a deterministic mock host, and in-memory router tests only.
- D-274: HTTP 451 and recognized safety-filter refusals fail over without resting the refusing member, including a numeric 451 on a stream error event and extracted remote-gateway error text. An exhausted non-regional 451 not already recognized as a policy refusal becomes hofund's own HTTP 400 safety refusal; an already-recognized one stays unchanged. Under Strict stays, a refusal asks only ready excluded members through the explicit safety-refusal continuation, and the answering member takes the stick. Cooling members are not asked, an unused continuation finishes its trace as the member that answered, and rate limits, quota, 5xx failures, and caps remain pinned to the strict account. Verified with deterministic fakes only.
- D-273: Z Code 3012 and WorkBuddy 11140 restricted-account responses now fail over to the next routing-group member on any classified 4xx, including the newer HTTP 405 suspension wording. The restricted account rests for two hours and sibling accounts of that provider are skipped; a plain unclassified 405 remains terminal. The sibling-skip classification is preserved across the subscription `ProviderAdapter` boundary. Verified with deterministic fakes only.
- D-263: Chat usage counts a top-level `reasoning_tokens` as reasoning when `completion_tokens_details` has no positive count, on a whole reply and on a stream. A positive count in `completion_tokens_details` is kept, and the two numbers are not added.
- #920 item 3: A Claude `/usage` failure that quotes a standard-error line, or an `is_error` result string, scrubs known secrets (an OAuth or setup token, a refresh token, or an API key value) with the same redaction as a process that ends early, before that text is clipped, so a token that crosses the clip is not returned.
- D-244: Under strict stays, a token count for a conversation that already has an answerer asks only that account. The local estimate is used when that account is not a candidate or cannot count, and no other account is asked. A new named session whose request already contains a reply stays on the account that history belongs to. The history map is kept in memory only. An unanswered subagent stays with its lead even when the routing does not spread new sessions.
- D-254: a Claude Code sign-in that has hit its limit, or whose `/usage` failed on a limit line or a session refusal since that account was in use, switches off until the named reset. A 401 stays put. `/usage` waits 45 seconds. A bridge rejection records the reset the vendor named, including a limit of one model family.
- D-254: the account in use and the session scan share one agent home, `CLAUDE_CONFIG_DIR` when set and otherwise `~/.claude`. A later bridge rate-limit event replaces an earlier rejected reset.
- D-254: Read Claude Code session-log growth from an open file handle so appended rate-limit refusals are detected on Windows while the writer remains open.
- D-260: a Google sign-in reply that says "Third-party quota reached" and "Resets in" rests that model until the written reset. A sibling model on the same account and another account are still asked. When every member still to be asked rests that way, the client gets 429 and `Retry-After` at the soonest reset, and that member is not asked. A token count during that rest does not ask the member; the next member counts, or the local estimate is used when none can. Verified with fixtures only.
- D-257: signing in to a saved ChatGPT API account again sends the client id that account was issued, `id_token_hint` when that ID token's expiry is more than a minute ahead, and `login_hint` from the saved email, and does not send `agent_name_hint`. A callback that names no client id is traded under the saved client. A callback that names another client, or an ID token for another subject, leaves the saved account in place. A new account still registers through `dynamic_agent_client`. `hofund subscription sign-in --user` names the saved account. Verified with a fixed clock and recorded token replies only.
- D-264: Zhipu and Z.ai Coding Plan providers read model capabilities from their region's models.dev plan catalog first, then `zhipuai`; preset-owned saved catalogs follow the selected region while a user-named catalog stays unchanged. Plan-only models now retain image input, context and output limits, and reasoning levels. The two GLM membership plan prices are skipped during ordered model-list and usage pricing: established models fall back to the metered catalog price even without a cached model-list price, plan-only models stay unpriced, unrelated catalogs do not borrow plan metadata, other coding-plan catalogs keep their published prices, and genuinely free non-membership models can still cost $0. Verified with fixture catalogs and loopback mocks only.
- D-264: GLM Coding Plan usage pricing now resolves the provider's ordered catalogs from the currently published configuration on every lookup. Providers added or retargeted after startup, including region or catalog-order changes, use the later metered price for established models and leave plan-only models unpriced without requiring a restart or cached model-list price.
- D-261: Over a Copilot model's image cap, earlier byte-identical image copies are now omitted first, using SHA-256 of each image content part and a placeholder that says the same image appears again later. The last copy is kept, including a copy in the latest user message, whose history copies are omitted. If the request is still over the cap, the oldest remaining image groups by last position are omitted with all their copies. Requests within the original image cap remain unchanged.
- D-253: an exact turn-key miss on the Claude bridge passes over Claude Code's local `No response requested.` reply when an idle run or a shelved session is waiting at the upstream's last reply with the same account, model, settings, system, and tools. That turn is the cancelled user message plus the local reply. A limit refusal or a busy retry keeps the cancelled message.
- D-255: Keep plugin accounts with remaining trial/bonus credits in routing tier 0 under Smart, Least used, and Pace, and keep them eligible for Drain; bought-credit ordering is unchanged.

### Security

- Lockfile: `url` 2.5.8 -> 2.5.9 (RUSTSEC-2026-0592, stack overflow in `Url::origin()` on nested `blob:` URLs) and `cookie` 0.18.2 -> 0.18.3 (RUSTSEC-2026-0594, private cookie nonces can repeat after `fork`); `form_urlencoded` 1.2.2 -> 1.2.3 comes with `url` 2.5.9. `cargo-deny` fails on both advisories without this update.

## [0.2.0-rc.3] - 2026-11-20

This is a third preview (prerelease) of v0.2.0. It carries the subscription,
routing, and provider alignment fixes listed below. The `v0.2.0-rc.1` and
`v0.2.0-rc.2` tags and GitHub Releases are left in place.

Real-account smoke tests for subscriptions are still pending and remain
Palmer's work: #304, #58, and #310. Request serving on this cut is verified
with mocks and loopback tests only; do not treat it as live-verified.

Held work is not in this cut: true 4.2–4.4 session recording, Library writes,
and MCP execution (awaiting a second security review); R-47b; R-47 k
credit-reset / `quota reset` / automatic consume; and the settings UI.
Desktop bundles (macOS `.dmg`/`.app`, Linux `.deb`/AppImage, and a Windows
NSIS installer) are produced by the existing tag workflow. As in
`v0.2.0-rc.2`, the Windows MSI is omitted because WiX requires a
numeric-only prerelease identifier. Signing and notarization run only when
repository secrets are set; otherwise the artifacts are unsigned.

### Added

- D-241: a remote gateway reads the preferred API on each `/v1/models` row, including a routing group, and asks that API when the client spoke a protocol the model does not serve. Chat the client itself sent stays Chat. `/v1/models` writes the same path under `preferred_api`. Those facts are stored on every key of the provider and seeded again from the saved model rows when the process starts, so a key that did not fetch the list and a restart that skips a fresh fetch still ask the preferred API and still return the catalog entry. Options `preferred_field`, `preferred_ask_header`, and `preferred_ask_value` read a remote that uses another field name or wants an extra list header. The ask header's value is sensitive, and a line break in its name or value is rejected. With those unset, only `preferred_api` is read and no extra header is sent. Codex is given the remote's own catalog from one `GET /v1/codex/models` with Codex's client version and the remote key. A failed catalog request keeps the entries already remembered. A handed catalog row is that remote object, with only the slug and one trailing ` via hofund` on the description changed. A remote id ending in `codex/<slug>` is this computer's whole cache row: the slug stays the gateway id and the description stays the cache text. A later request matches the exact listed id, or the single longest slash-bounded suffix, and stays on the client's protocol when two suffixes are the same length.

- D-235: A Copilot model whose `supported_endpoints` include `ws:/responses` sends Responses turns on that API's WebSocket at `<api>/responses`. The next turn on the kept socket sends `previous_response_id` and only the new input, including when a sealed reasoning item is absent. The handshake carries the account headers and does not send `OpenAI-Beta` or `X-Initiator`. A refused handshake, a clean close before a terminal event, a gap between frames longer than the pooled HTTP read budget, an accumulated turn over 8 MiB, or an error frame that names no HTTP status uses HTTP and does not keep the socket. An error frame that names an HTTP status is classified like the HTTP body, including `error.code`, quota, credit, and a written retry delay. Idle (10 minutes) and max age (55 minutes) close on the host clock with no later turn, and one account keeps at most 32. The dial reuses the pooled HTTP client's proxy decision for that origin, including `HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY`, and it honors `NO_PROXY` and the CGI `REQUEST_METHOD` exception. When that decision is a proxy, the socket is refused before any connection, so `Authorization` is not sent on a direct socket the HTTP path would not use. A kept socket's idle time starts when the turn finishes. Skipped frames count toward the read budget and the 8 MiB cap. Verified with a loopback socket only.

- D-239: a routing group can use `fit` (small context first). The order starts from Smart, then stably sorts by band and by known context window, smallest first, with an unknown window last. A member set to be used first stays ahead while it still has room, and a member all but used up stays behind the others. On `fit`, a member whose known window is no larger than the request estimate is left out of the ask list. An unknown window stays with the members that may hold the request. A left-out member is asked only when the final try failed for quota or credit, no earlier fitting member failed for another reason, and that try was not a refusal, a passing retry, or an overflow. A passing failure of the member that is currently last is replanned and does not by itself block that resort. A left-out member already resting for quota or credit is not asked. Deleting a provider demotes `small-first` and `small-context-first` the same way as `fit`. When every member is too small, the strategy order is unchanged and those members are asked. Other strategies still ask in order and skip a known-too-small member only after an overflow. Config also accepts `small-first` and `small-context-first` as names for `fit`. `fit` is refused on a route that is not a group, and on a subscription's own routing. The TUI cycle lists it after `drain` and labels it small context first.

- D-230: `GET /v1/realtime` and `GET /realtime` upgrade to a WebSocket and pass the Realtime session through to a provider that declares a realtime endpoint. The upgrade uses the same front guards as other routes. A gateway key's spending reservation is held until the socket closes. Frame size, idle time, and close are bounded. Verified with a local fake upstream only.

- D-224: an inbound Chat or Responses `service_tier` of `fast` is sent upstream as `priority`. Ultrafast is offered for a ChatGPT-backend GPT Sol, Astra, or Terra model, for the public Responses models that host accepts, or for a member configured ultra. A public Chat hop does not offer Ultrafast, and there is no per-account plan list. Responses to `api.openai.com` send `ultrafast` only for the models that host accepts; Chat Completions to that host send `priority` instead. Usage prices the served tier: Fast and `priority` at 2x, Ultrafast at 6x, Flex at 0.5x, and `default` or `standard` at 1x. Only an empty or `auto` served tier falls back to the tier that was sent. The ChatGPT backend's placeholder `auto` and `default` are dropped before that choice. GPT-6.2 Astra and Sol Ultrafast is 3x through 2027-02-28 Pacific and 6x from 2027-03-01, using the call's day. A Responses stream keeps the served tier from `response.created` or `response.completed`. A member sent Ultrafast is labelled Ultrafast. A ChatGPT-backend hop drops any tier other than `priority` and `ultrafast` before the sent tier is recorded, so a dropped `flex` ask is the list price. `Fast` in any ASCII case is the same word as `fast`. A subscription hop reports the backend base its plugin calls through the adapter's attempt route, so those ChatGPT-backend rules apply when the provider row's base URL is empty. The production Codex base is `https://chatgpt.com/backend-api/codex`. An OpenAI-compat key reports the base it posts to. A ChatGPT API subscription reports the Responses base it posts to and the codec it speaks, so the public Responses host rule applies while the member label stays `subscription`. The plugin wire path keeps `service_tier` across a cross-protocol strip and adjusts the encoded body for the target base and codec before posting. A served `service_tier` survives a fold, a unary decode, and a cross-protocol stream, and the word on `response.completed` replaces an earlier word, so a reply of `default` is billed at the list price.

### Changed

- D-227: A Claude Code request through the Claude subscription bridge that carries
  an advisor server tool (`advisor_20260301`) for a model this gateway serves
  keeps the client's instructions, offers an advisor tool beside the client's
  tools, and answers a call by asking that model through normal routing as the
  original gateway key. The client is told `server_tool_use` and
  `advisor_tool_result`. One reply may ask three times; the next is
  `max_uses_exceeded` and is not asked. A model this gateway does not serve is
  left out and those instructions are voided. One advisor call waits at most
  55 seconds. A dashed Claude id is sent as the provider spells the same
  model. Relayed Messages replies record each `usage.iterations` advisor call
  as its own usage row, kind `advisor`, priced at the advisor model. The
  executor row is unchanged. Each reply, including a parked continuation,
  starts that three-use count again and shows the advisor that reply's
  conversation. A client tool already named `advisor` stays a client tool.
  An advisor row stores the iteration's input as uncached input and keeps
  cache reads on their own column. A model this gateway routes stays offered
  when the caller's list would refuse it; that call then fails without asking
  the provider. A dashed id that two providers both spell matches neither.
  The nested ask records usage with the answering hop's protocol, so an input
  total that already includes cache reads is not billed again. That row is
  priced as the upstream model, not the route id, and it does not store
  `advisor` as the client agent. The row keeps the provider key and the
  sent and served service tiers, so a list price is multiplied the same way
  as a direct call. An empty answer is recorded with the 502 `ask` returns.
  A web search
  on that run, when one is installed, uses the same original key. Refs D-227.
- D-222: Antigravity can be used through the user's own local `agy` CLI (`plugins/subscriptions/agy-cli`). Saved accounts move into per-account homes, a written quota reset rests the requested model, and a verification refusal rests the account for 30 minutes. hofund does not download the CLI. A quota or verification sentence inside a success JSON field, or on standard error, rests the same way. A Code Assist body that names its reset keeps that duration on `reset_after`, a Google sign-in 429 with a model-pool sentence is a quota error, and an import probes each account through that account's proxy.
- D-221: Antigravity sign-in, import, and add are refused for new accounts because Google suspends accounts it sees on Code Assist through Antigravity's own client. Saved accounts keep working and show a risk line. The message points at the agy CLI route. Gemini CLI is unchanged.
- D-225: Generative Language and Vertex model paths escape each slash-separated segment and keep the slashes. An id with an empty, `.`, or `..` segment is escaped as one segment.

### Fixed

- D-242: A Codex subscription function tool whose `strict` is omitted, or is not a boolean, is sent with `strict` false. A tool that asks for strict keeps that flag when its parameters still compile under the ChatGPT backend's strict rules, with each `oneOf` keyword rewritten as `anyOf`. A schema those rules still refuse (a keyword they do not take, a lookaround in a pattern, an open object, a property left out of `required`, a root that is not an object, or a node with only `const`) is sent as written with `strict` false. Function tools nested in a namespace, and in `additional_tools` or `tool_search_output` input items, are included. A Chat or Messages client's `strict` survives the cross-protocol extension strip onto the Responses tool, so this check sees it. An omitted `strict` stays omitted until this pass.
- D-240: An Azure key that has answered a request and then receives HTTP 401 rests suspended for 24 hours, and the deployment list is probed in the background until it answers. A key that has never answered, or whose secret was replaced, keeps the generic 401 rest and is not probed. A one-key provider is answered only by a ledger row for the key it is using now, and only when that row is still inside the 30-day window, so an older key's success does not suspend the replacement after restart. A model success that started before the secret change does not mark the replacement answered, whether the same member is kept or the member is removed. Dropping that active suspended rest emits a recovery. A later successful request lifts that member-wide rest. A ledger key id that is not kebab-case is skipped. The plugin sets a vendor-neutral `subscription_suspended` flag; the router does not match vendor wording.
- D-237: A Claude `/usage` reading that still has no 5-hour window after the weekly used share rose inside five hours no longer shows an invented 5-hour window at 0%. The omission is kept in the account directory and stays out across a restart until a reading includes a real 5-hour window. An account not yet seen used still shows the unstarted 5-hour window. A previous no-meters aside does not count as that omission, so a later idle week still shows the unstarted window. A header that is not a whole `/usage` reading does not count either, so it does not hide the saved reading. The decision uses the windows and time from before `/usage` runs. The reading file is created mode `0600`, and a failed save is returned.
- D-238: When Claude `/usage` text draws no window, 5-hour and weekly figures are taken from Claude Code usage-report rows (`limits` kind `session`, `weekly_all`, and model `weekly_scoped`, with an RFC3339 reset). A usage-based seat (`.claude.json` `oauthAccount` `seatTier` `enterprise_usage_based` or `billingType` `usage_based`), or a report whose `limits` array is empty, shows "5-hour and weekly limits: None on this plan" as a non-blocking aside. A null `limits` or `rate_limits` value stays a failed read. A JSON result with `is_error: true` is a failed read and is not parsed, including on a usage-based seat. Empty or whitespace-only command output is that same kind of failed read, and quotes a clipped standard-error line when one was printed. A JSON success with an empty `result` string, and the subscription sentence, still show the aside. A reset time that contains an ASCII control character is skipped. A model name or currency that contains an ASCII control character is skipped. A plan that reports no meters is not also given an unstarted 5-hour window.
- D-221: A menu snapshot reads enrollment risk lines from the running daemon, or from a cache filled on the 60-second tick while the gateway is stopped, and does not build plugins or run account hooks. A refused discovered account is recorded and the pass continues; only an explicit add whose rows were all refused returns that refusal. A stored Antigravity account can be signed in or imported again. A new account id stays refused. The risk line shows whenever the plugin is configured.
- D-221: A sign-in for an account id that is not stored stops after the token exchange and userinfo, and does not call Code Assist setup. An imported refresh token is written only when userinfo confirms a stored account id, or when that same refresh token is already stored. A caller-supplied email does not attach an unverified token to a saved account.
- D-230: A Realtime `wss` dial uses rustls with the native root store. The dial follows the member's HTTP proxy and fails closed when that proxy cannot be used, without opening a direct socket. Each key tick also applies the issuer's limits, and a key-store or usage-ledger error closes the session. Usage is priced on the hop's wire id. Cached tokens are clamped to the input count. A `response.done` whose status is `failed` is recorded as HTTP 500.
- D-234 follow-up: A restricted rest is never armed for a background model-list probe, and a probe cannot lift it, so the two-hour rest stands. Unserved-model wording does not narrow that rest to one model. Protection wording on a restricted account does not skip that provider's other keys. A token-count failure passes the response headers to the target classifier.
- D-226: Using a Claude setup-token account writes `CLAUDE_CODE_OAUTH_TOKEN` into Claude Code's `settings.json` `env`, records only a hash of that value, and leaves a value it did not write in place. Switching to a signed-in account or removing the setup-token account takes the recorded value out. A pasted setup-token line in the Claude import is stored as a setup-token account. The settings file is mode `0600` and written atomically. The token is not logged. Other subscriptions return unsupported for that paste, including a line of any length or bytes that starts with `sk-ant-oat`, and make no token request; when Claude is not configured, import fails without sending the token. Automatic switching does not select a setup-token account and does not move off one that is in use. `next` stays on it too. Naming another account, or signing the setup-token account out, still switches. An explicit Use still writes the env token, including when the live profile cannot be named, and leaves the credentials file unchanged. `keep_login` uses the first switchable account. A remembered return account that is not switchable is ignored. Clearing it leaves the hash record in place when the home or the value is not the one recorded. A mixed paste, including a JSON array that contains a setup-token and other entries, stores the setup-token account and then reports the first other entry, because import returns one result.
- D-229: A Copilot turn refused with "too many images" learns that model's cap, replaces the oldest images outside the latest user message with a placeholder, and sends the turn once more. Later turns for that account and model are fitted before they are sent. A turn already within the cap is unchanged. When the latest user message alone is over the cap, the refusal is returned unchanged. The cap comes from the refusal, not from the model list.
- D-234: A Claude subscription account, or an Anthropic API key whose upstream is
  `api.anthropic.com`, that the vendor has turned off rests for two hours and
  the routing group asks the next member, including that provider's other
  accounts. The same words from a relay fail over with an ordinary failure
  rest, not a restricted one. A token count, and a 200 body that is only an
  error object, rest the same way. Extra-usage and Usage Policy refusals are
  unchanged. Refs D-234.
- D-228: A translated request with several tool results and a following user turn keeps every result in one user message, paired with its call. Consecutive Messages user messages that each carry a tool result stay separate, and the mend pairs calls across that run without dropping either message's other members.
- D-228: An unanswered call in a consecutive user run gets its synthetic error result in the first user message, after that message's real results and before its other blocks. A later plain-text user message is not rewritten to hold that synthetic. A system or developer message held before the run is released after the last result, before a trailing text-only user message. A message that receives those blocks and arrived without `content` keeps its other members.
- D-228: Filling tool-turn thinking keeps existing thinking and tool-use blocks, including on an earlier assistant when a later turn is the one rewritten. A Messages client's consecutive user or assistant turns stay separate, so each keeps its own cache mark. A folded developer reminder joins only an adjacent tool-result user and keeps both messages' other members. Text parts of that developer message are joined with a newline. Non-text parts of that developer message are not copied onto the reminder.
- D-228: A Chat, Responses, or Generative request encoded for Messages folds a system or developer message that follows a reply into a user `<system-reminder>`, and never writes `developer` inside `messages`. A Messages client's own inline `system` turn stays in `messages`. A system message is encoded without `content` only when it arrived without that field. A developer message rewritten to system because its text is empty still carries `content` when the field was present, including an empty array or only non-text parts. A user or assistant message is still encoded with `content`. Duplicate call ids in one assistant message are reserved before a suffix is minted, so a real `{id}__2` is not taken by an earlier copy.
- D-228: A Messages body whose `tool_use` and `tool_result` blocks do not pair is
  mended before it is sent to a Messages upstream. An unanswered call gets a
  synthetic error result, a result with no call becomes the text it said, a
  second answer is dropped, and a result after other content moves first. A
  body that already pairs keeps its bytes.
- D-236: That same mend moves blocks an assistant wrote after its first
  `tool_use` to before the call (thinking stays, empty text is dropped), moves
  a system or developer message that sits between a call and its results to
  after the results, and keeps two calls that share an id as two ids, each
  answered. Passthrough, `count_tokens`, and translated Messages builds use
  this one pass. A body that is already in shape is not serialized again.
- D-232: GPT-6.2 and later send Codex instructions, the client's instructions,
  and a translated system prompt as leading `developer` input items, with no
  top-level `instructions`. Older models stay unchanged. This applies to the
  Codex subscription, the ChatGPT API subscription, and translated Chat and
  Anthropic Responses encodes. When Codex's instructions are unknown or blank,
  the client's text still moves to the front of `input` on those models.
- D-223: A Codex Responses Lite turn (an `additional_tools` input item, or
  `reasoning.context` of `all_turns`, and no client instructions) is sent
  without added instructions and without `reasoning.summary`. A Lite turn on
  GPT-6.2 keeps those Lite rules. A socket replay removes `x-codex-turn-state`
  from `client_metadata` once it is a header, and that socket keeps the last
  reply's state for the same turn id. A per-account store for pooled turns is
  not implemented.
- D-231: ChatGPT model-list asks, the `version` header, the User-Agent, and
  device sign-in name the newest plain `x.y.z` among a `0.169.1` floor, npm's
  latest `@openai/codex` release (read at most every six hours on the host
  monotonic clock), and a newer
  Codex client already seen on a routed request. A model list asked at an
  older client version is fetched again. Refs the reference.
- D-233: Claude bridge plans set `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0` and `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=0` unless the process environment already sets that name, and a WSL plan lists both on `WSLENV`, so a client tool is waited on instead of being moved to the background or aborted at the idle limit. One-off runs are unchanged.

## [0.2.0-rc.2] - 2026-11-18

This is a second preview (prerelease) of v0.2.0. It exists so the tag
workflows can publish the CLI/daemon archives, shell and PowerShell
installers, and the universal macOS archive that `v0.2.0-rc.1` missed.
The `v0.2.0-rc.1` tag and GitHub Release are left in place.

Real-account smoke tests for subscriptions are still pending and remain
Palmer's work: #304, #58, and #310. Request serving on this cut is verified
with mocks and loopback tests only; do not treat it as live-verified.

Held work is not in this cut: true 4.2–4.4 session recording, Library writes,
and MCP execution (awaiting a second security review); R-47b; R-47 k
credit-reset / `quota reset` / automatic consume; and the settings UI.
Desktop bundles (macOS `.dmg`/`.app`, Linux `.deb`/AppImage, and a Windows
NSIS installer) are produced by the existing tag workflow. The Windows MSI is
omitted from this prerelease because WiX requires a numeric-only prerelease
identifier. Signing and notarization run only when repository secrets are
set; otherwise the artifacts are unsigned.

### Changed

- The detailed `[0.2.0-rc.1]` change list moved to
  `docs/changelog-v0.2.0-rc.1.md`, so cargo-dist announcement bodies stay
  well under Linux `MAX_ARG_STRLEN` (~128 KiB).
- The Release workflow reads GitHub Release notes from `dist-manifest.json`
  with `jq` into a file, and no longer passes the announcement body through
  an environment variable or process argument. The plan job also writes a
  slim manifest (artifact matrix only) to `GITHUB_OUTPUT`.
- The Release desktop attach job sets `GH_REPO`, surfaces `gh` stderr, waits
  for the CLI release, and creates a prerelease stub if that workflow did
  not, then uploads desktop bundles and a combined `SHA256SUMS`.

## [0.2.0-rc.1] - 2026-11-18

This is a preview (prerelease) of v0.2.0. Real-account smoke tests for
subscriptions are still pending and remain Palmer's work: #304, #58, and
#310. Request serving on this cut is verified with mocks and loopback tests
only; do not treat it as live-verified.

Held work is not in this cut: true 4.2–4.4 session recording, Library writes,
and MCP execution (awaiting a second security review); R-47b; R-47 k
credit-reset / `quota reset` / automatic consume; and the settings UI.
Desktop bundles (macOS `.dmg`/`.app`, Linux `.deb`/AppImage, and a Windows
NSIS installer) are produced by the existing tag workflow. The Windows MSI is
omitted from this prerelease because WiX requires a numeric-only prerelease
identifier. Signing and notarization run only when repository secrets are
set; otherwise the artifacts are unsigned.

The cargo-dist Release workflow did not attach CLI/daemon archives on this
tag, because the original change list in this section exceeded Linux
`MAX_ARG_STRLEN`. The full change list is in
[docs/changelog-v0.2.0-rc.1.md](docs/changelog-v0.2.0-rc.1.md). The
`v0.2.0-rc.1` tag and GitHub Release were left in place; later prereleases
starting with v0.2.0-rc.2 carry the CLI archives.

## [0.1.0] - 2026-10-25

Early preview of the local gateway. The notes below are everything on `main`
at this tag.

### Added

- Canonical chat IR in `hofund-ir`: messages, content parts (text, image,
  tool call, tool result, thinking), tool definitions and tool choice,
  generation parameters, usage, stop reasons, typed stream events, and an
  extension bag that preserves unknown JSON fields.
- `hofund-codec` implementations for Anthropic Messages, OpenAI Chat
  Completions, and OpenAI Responses, including SSE streaming. See
  [docs/codecs.md](docs/codecs.md).
- `hofund-quirks` profiles, `hofund-provider` adapters for Chat, Responses,
  and Messages, and `hofund-router` ordered failover before the first stream
  event.
- Local HTTP server on `127.0.0.1:3425`, TOML configuration, a bearer token
  file, SQLite usage rows, and the `serve`, `config check`, `token show`,
  `token rotate`, and `usage` commands.
- Coding-agent adapters for Claude Code, Codex CLI, Gemini CLI, and OpenCode.
  `hofund agent list`, `hofund agent connect`, `hofund agent disconnect`, and
  `hofund agent status` point an agent at the local gateway. Each edit is
  backed up once, written by rename, and restored byte for byte.
- Streamed Responses `encrypted_content` is kept on the thinking part and
  written back on encode.
- `GenerativeLanguage` codec for `generateContent` and
  `streamGenerateContent`, including function calls, system instructions,
  thinking parts, thought signatures, and usage metadata. The gateway serves
  `POST /v1beta/models/{model}:generateContent`,
  `POST /v1beta/models/{model}:streamGenerateContent`, and
  `GET /v1beta/models`. Authentication accepts the bearer token,
  `x-goog-api-key`, or a `key` query value, compared in constant time.
- Menu-bar app `hofund-desktop` (Tauri v2) at `apps/desktop`. It starts and
  stops the local gateway, shows the listen address, toggles each catalog
  agent through `hofund-agent`, prints the usage table, opens the config
  file, and quits. The config path is `HOFUND_CONFIG` or
  `hofund/hofund.toml` under the platform config directory.
- Release workflow for the `hofund` CLI (cargo-dist: macOS arm64, x86_64, and
  universal; Linux x86_64 and aarch64; Windows x86_64) and for Tauri bundles.
  Archives carry SHA-256 checksums. Code signing and notarization run only
  when the repository secrets are set.
- Initial Cargo workspace scaffold for the hofund local LLM gateway, with
  library and binary crates, architecture checks, CI, and repository
  hygiene files.
- `docs/feature-parity.md` records each capability with a stage (`core` or
  `auxiliary`) and the hofund status of that row.
- `docs/roadmap.md` records the two-stage plan. The terminal UI and every
  subscription capability, including the Claude bridge, are stage 1.
- A route member may name the model sent to that provider:
  `upstreams = [{ provider = "...", model = "..." }]`. A string is still a
  provider id.
- Clients may call a route by its name, by `provider/model`, or by a wire
  model that exactly one provider serves. A wire model served by several
  providers is an error that names the candidates.
- `GenerationParams.effort` maps reasoning effort across Chat Completions,
  Responses, Messages, and Generative Language.
- `POST /v1/messages/count_tokens` and
  `POST /v1beta/models/{model}:countTokens`. A Messages upstream is asked to
  count. Otherwise the body is a character estimate with `estimated` set.
- `GET /health` returns `{"status":"ok"}` with no authentication and no secrets.
- `hofund --help`, `-h`, and `help` print usage for each command.
- `examples/hofund.toml` documents `[[providers]]` and `[[routes]]`.
- The gateway accepts `x-api-key`, compared in constant time, on every routed request.
- Provider presets. `[[providers]]` may set `preset` instead of `kind`, with
  optional `region`, `protocol` (`chat`, `responses`, `messages`, or
  `generative`), and `base_url`. The catalog is `crates/provider/presets.toml`.
- `kind = "generative"` posts the Generative Language API:
  `models/{model}:generateContent`, `streamGenerateContent?alt=sse`, and
  `countTokens`. The API key is sent as `x-goog-api-key`. `preset = "google"`
  uses that adapter. `protocol = "chat"` on that preset keeps the
  OpenAI-compatible root.

### Changed

- Responses usage objects no longer include `cache_write_tokens`. The counter
  stays on the internal usage object. `input_tokens_details` carries
  `cached_tokens` only.
- The CLI default config path matches the menu-bar app: the platform config
  directory, then `HOFUND_CONFIG`, then `--config`.
- `hofund config check` prints the path it read, and `config ok: <path>` on success.
- `hofund token rotate` is picked up by a running gateway on the next request.
  Connected agents have their token keys rewritten. When that edit fails, the
  command prints `hofund agent connect <id>`.
- `hofund agent disconnect` restores a file that still matches what connect
  wrote, and otherwise restores only the values hofund wrote. A value the
  user changed is kept, with a warning. Later keys stay. `--force` restores
  the backup bytes. A second disconnect exits 0.
- `hofund usage` prints local time and a totals line.
- `hofund agent connect --dry-run` redacts the bearer token in the diff.
- Connect warns when the tool's configured model has no route.
- OpenCode follows `XDG_CONFIG_HOME` when that variable is set.
- Disconnect removes empty directories that connect created.
- OpenCode writes `provider.hofund` with npm `@ai-sdk/openai-compatible`,
  `options.baseURL`, `options.apiKey`, and `models`.
- A provider and a route member may set `max_output_tokens`. Messages encode
  uses 16384 when the client, the route, and the quirk profile all omit a cap.
- A Responses usage object always includes `input_tokens`, `output_tokens`,
  `total_tokens`, `input_tokens_details`, and `output_tokens_details`. A
  missing counter is 0.
- Generative Language `usageMetadata` always includes `promptTokenCount`,
  `candidatesTokenCount`, and `totalTokenCount` when usage is present.
- `function_call` items no longer include an empty `encrypted_function_args`
  array.
- `hofund agent connect opencode` says that `models` stays empty because this
  build does not ship a model catalog and does not copy route names.

### Fixed

- `hofund agent disconnect` no longer deletes a key that existed before
  connect, and it no longer removes the backup before the restored file is
  read back.
- A Responses request with `namespace`, `web_search`, or `custom` tools is
  accepted. The same protocol forwards the tools unchanged. A different
  protocol maps a custom tool to a function with one string input, flattens
  a namespace, and drops a tool it cannot map after a warning.
- A locally rejected request records the model named in the body.
- Client-facing Responses ids start with `resp_`, each output item has its
  own id, and a non-streaming Messages id starts with `msg_`.
- A Chat Completions stream forwards a usage chunk only when the client set
  `stream_options.include_usage`. That chunk has an empty `choices` array,
  and every chunk keeps `id` and `model`.
- `examples/hofund.toml` points the sample backup provider at a local URL.
- A system text that is only `x-anthropic-billing-header` is removed when the
  request is sent to a different protocol.
- The extension bag is forwarded only when the inbound and outbound protocols
  are the same. A different protocol drops it, including caller identifiers
  such as `metadata.user_id`, and drops raw stream events that the decoder
  does not model.
- A streamed Chat Completions upstream body sets
  `stream_options.include_usage`, so usage is returned and recorded.
- Anthropic streams write `message_start.message.usage`, prefix message ids
  with `msg_`, and emit `content_block_stop` before the single `message_delta`
  that carries usage.
- Responses streams emit `response.output_text.done`,
  `response.content_part.done`, a `response.output_item.done` item with `id`
  and `content`, a terminal `output` array, and `sequence_number` on every
  event.
- `token rotate` updates a running gateway, which previously kept accepting
  the previous token until restart.
- Disconnect keeps edits the user made while connected, such as a Codex
  `sandbox_mode` key.
- A Messages upstream accepts a request that omitted `max_tokens`. A request
  the codec cannot encode is HTTP 400 `invalid_request_error`.
- A namespaced tool call comes back as `namespace` plus the original name.
  A custom tool call comes back to a Responses client as `custom_tool_call`
  with the raw input.
- A Responses stream uses the same `output_index` as
  `response.completed.output`.
- `POST /v1/messages/count_tokens` does not send `max_tokens`.
- Responses item ids are unique. A same-protocol reply keeps the upstream
  item id. A unary output item has `id` and `status`.
- A dropped tool is warned once per name per process.
- Chat usage includes `prompt_tokens`, `completion_tokens`, and `total_tokens`
  whenever a usage object is written. A missing input or output is 0. A
  missing total is their sum.
- A Messages stream's usage is merged across `message_start` and
  `message_delta` when it is encoded for Responses, Chat Completions, or
  Generative Language. An output-only delta no longer drops the input tokens.
- Responses history items `custom_tool_call` and `custom_tool_call_output`
  sent to Chat or Messages keep the call id, the raw input, and the output.
  The call is an assistant tool call with arguments `{"input": <raw>}`. The
  output is a tool result.
- A namespaced `function_call` in history is sent upstream under the flattened
  name declared for that tool.

### Known limitations

- Tested only against mock upstreams. This release has no live OpenAI,
  Anthropic, or Gemini runs.
- Gemini CLI and OpenCode were verified via config only.
- The Codex `apply_patch` path was verified via HTTP only.
- Binaries are unsigned.
- The desktop tray app was not smoke-tested.
- Stage-1 parity is incomplete. See [docs/feature-parity.md](docs/feature-parity.md).
