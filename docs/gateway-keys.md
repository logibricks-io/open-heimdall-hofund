# Named gateway keys

Named gateway keys let each client have its own credential, identity, model
scope, and account scope. They are separate from upstream provider credentials.

## Storage and the default key

The store is `caller-keys.json` beside the configured token file. It is written
atomically with owner-only permissions (`0600` on Unix). Each read-modify-write
sequence holds an in-process lock and an exclusive file lock on
`caller-keys.json.lock`, so the CLI and the daemon never overwrite each other. A running gateway rereads it for
authentication, so rotation, disable, removal, and scope edits take effect
without a restart.

Each secret is stored readable, as the reference stores it
(`reference: internal/access/access.go`), so a key can be copied again after it
was made: `hofund gateway-key copy <id>` and the control `copy` action return
it. The store file is the protection: every write leaves it owner-only, even
one that was loosened by hand. Listings never carry a secret, only a masked form
(`hofund_key_…` and the last six characters for a generated key, `…` and the
last four for a key of your own of 12 or more characters). Constant-time
comparison covers every stored key before a request is accepted or refused.
`Debug` output of a key record redacts its secret. Backup exports carry the
secrets, so a restore brings working keys back.

On first load, the existing single token becomes the default sharing key. Its
secret does not change, so connected agents keep working. `hofund token show`
still prints that token, and `hofund token rotate` rotates the default key and
its legacy token-file mirror. The default key cannot be made read-only. Removing
it leaves a tombstone, with no secret, so migration does not resurrect it.

## CLI

```sh
hofund gateway-key add "Remote laptop"       # prints the new secret
hofund gateway-key add Family --key -       # reads an existing secret from stdin
hofund gateway-key add Display --read-only  # may read stats, never call models
hofund gateway-key list
hofund gateway-key list --json
hofund gateway-key rotate <id>
hofund gateway-key copy <id>                 # prints the key's secret again
hofund gateway-key rename <id> <name>
hofund gateway-key on <id>
hofund gateway-key off <id>
hofund gateway-key remove <id>
hofund gateway-key move <id> <place>         # place is counted from 1
hofund gateway-key models <id> primary/model-a 'secondary/*'
hofund gateway-key models <id> all
hofund gateway-key accounts <id> primary/<key-fingerprint>
hofund gateway-key accounts <id> all
hofund gateway-key quotas <id> on
hofund gateway-key access <id> full
hofund gateway-key issuer <id> on
hofund gateway-key limit <id> --per day --tokens 1000000
hofund gateway-key limit <id> add 5h --cost 2.50 --cache-reads
hofund gateway-key limit <id> --per hours --hours 5 --tokens 200k --index 2
hofund gateway-key limit <id> remove 1
hofund gateway-key reset-limit <id> [N]
```

The list columns are `ID NAME STATE GATEWAY KEY LIMIT MODELS ACCOUNTS ACCESS`,
as in the reference, plus `UNTIL` until expiry joins `STATE` in P0-4d.
`GATEWAY KEY` is the masked form.
`models`, `accounts`, `quotas`, `access`, and `issuer` also work without a value
to show the current setting. `access <id> read-only|full` changes that access,
and `add <name> --read-only` sets it at creation. The default sharing key must
retain full access.

Authentication accepts `Authorization: Bearer <key>`, `x-api-key`,
`x-goog-api-key`, or `?key=<key>`. One enabled match is enough. Missing, unknown,
disabled, and removed keys receive 401. Existing loopback behavior is unchanged:
hofund still requires a key on loopback.

## Read-only keys and the key page

A read-only key may use only `GET` or `HEAD` on `/health`, `/me`, `/v1/hofund/me`,
`/v1/hofund/quotas`, and `/v1/hofund/quotas/history`. Every other route is
refused before its handler or any upstream call with HTTP 403 and error code
`gateway_key_read_only` in a `permission_error` envelope. Model lists and calls,
token counts, writes, inbound surfaces, and both Responses WebSocket upgrades
are refused. Route construction records every method and path; the
`r2_every_registered_route_is_allowed_or_refused_and_exercised` test fails when
a route is unclassified, and exercises both lists. Restoring full access takes
effect on the next request.

The reference also allows root information, quota cards, usage summary and
request rows, limit, concurrency, and its own MCP inventory. Hofund has no
corresponding routes yet, so this change does not invent them. The allow list
will expand only with those real routes.

`GET /me` serves the same static English page to everyone, without a key. Its
CSP allows connections only to the same origin, forbids forms and framing, and
loads no external resource. The page keeps a typed key only in tab
`sessionStorage`, sends it as a bearer header, and offers placeholder-by-default
setup for Claude Code, Codex, OpenAI-compatible clients, and a curl test.

`GET /v1/hofund/me` requires an enabled named key and answers with
`Cache-Control: no-store`. A `key` query pair is refused with 400 so a credential
does not enter an address. The response contains only the caller's `name`,
`readOnly`, `limits`, `usage`, `topModels`, `models`, and the gateway `version`.
Usage covers today, seven days, and thirty days; the eight most-used models are
ranked from only that key's rows. Limits include their current RFC 3339 window
and token use. Allowed models are the scoped model-list ids, or none for a
read-only key. No secret, account, other key, or provider credential is exposed.
The current ledger has no cache or price columns, so those response counters are
zero. A legacy token without a named-key store, an unknown key, and a disabled
key receive 401.

## Model and account scopes

Model patterns are `<provider>/<model>`, `<provider>/*`, or `group/<name>`.
Matching ignores letter case. `*` matches any run of characters, including `/`.
A group pattern authorizes the group route, not direct requests for its members.
A model outside scope receives 403 before any provider is contacted, naming the
key and its allowed patterns. `/v1/models` and `/v1beta/models` contain only
models the caller may use. Unauthorized failover candidates are skipped.

Account patterns are `<provider>/<account-id>` or
`<provider>/<provider-key-fingerprint>`. They are validated when set. Providers
not named in the account list keep their ordinary access. If a key has accounts
but no model patterns, its effective model scope is only those accounts'
providers. Account scopes apply to ordinary calls, token counts, and raw
forwarded calls. Store helpers rewrite exact account references after an account
rename and exact model references after a model move.

Usage rows additively gain `caller_key_id` and `caller_key_name`, including rows
for local refusals. The control usage response carries the same fields.

The `quotas` flag is persisted and exposed on the authenticated identity. It
will select permitted provider/account quota cards when that API lands.

## Spending limits

A key can hold up to five independent limits. Each has a token cap, a cost cap,
or both. A limit with neither cap is removed. Periods are `day`, `week`, `month`,
`days` (1 to 3650 days), and `hours` (1 to 8760 hours). Calendar windows use
local midnight, Monday, or the first of the month. Repeating cycles use absolute
24-hour days or hours from `since`; changing only caps does not restart a cycle,
and a cycle written without `since` starts from the Unix epoch. Two limits on
one key cannot describe the same window.

`reset-limit` sets `since` without deleting usage. A calendar reset excludes
earlier calls but keeps the calendar window's end; a cycle starts again from the
reset. With several limits, reset one by its one-based number or all by omitting
it. `limit <id> off` removes every limit, and `remove <n>` removes one. The
positional `list`, `add`, `reset`, `off`, `remove`, `<N>d`, and `<N>h` forms are
also accepted. The CLI prints stored caps after a mutation and summarizes them
in the key list's `LIMIT` column.

Tokens are uncached input, output (reasoning included), and cache writes;
`--cache-reads` also counts cache reads. SQLite stores the protocol's original
input total and a separate uncached-input count, plus cache counters, so budget
reconstruction does not change ordinary usage reporting. A call counts in the
window where it started. Sums are rebuilt from `caller_key_id` after a restart;
local rejected rows do not count, while failed upstream calls keep any tokens
they recorded.

Before dispatch, each in-flight request reserves one quarter of its decoded body
bytes plus the key's mean output per call in that window. Every limit must have
room. Admission and reservation updates share one lock, so parallel requests
cannot all pass on a stale sum. A reservation stays in its original window and
is released after the response or stream settles. A spent key gets a
protocol-shaped 429 before any upstream call, naming the key, usage, cap, and
reset. The response includes `Retry-After`, `X-Should-Retry: false`, and
`X-Hofund-Limit-Reset`; when several limits are spent, the one resetting last is
named. Token counting routes are not limited.

`GET /v1/hofund/limit` authenticates the caller and returns only that key's
status. With no limits it returns `limited: false`. Otherwise `limits` contains
each limit's calls, tokens, cost, unpriced calls, reservations, caps, remaining
amounts, start/reset, and spent state, while `limit` is the binding one. Expiry
and issuer-limit fields arrive with short-lived keys in P0-4d.

There are no model prices yet. Cost limits are accepted, but every forwarded
call is currently unpriced: it adds zero cost and increments `unpriced`, so an
unpriced call cannot exhaust a cost cap. This is the reference's `priced=false`
path (`reference: internal/budget/budget.go`). Priced accounting remains with
the model-price work.

## Control API and backup

The authenticated control plane provides:

- `GET /v1/hofund/gateway-keys`
- `POST /v1/hofund/gateway-keys` with an `add`, `rotate`, `rename`, `on`, `off`,
  `remove`, `move`, `models`, `accounts`, `quotas`, `access`, `issuer`, `limit`,
  `reset_limit`, or `copy` action; add, rotate, and copy return the secret
- `GET /v1/hofund/gateway-keys/export` for complete backup records, secrets
  included
- `POST /v1/hofund/gateway-keys/restore` to replace the store from those records

`hofund-control-client` has typed methods for all four endpoints. Secret-bearing
requests and responses redact secrets from `Debug` output.

## Later phases

Expiry, issuer ids, and issuer permission flags are stored so older files
remain forward-compatible. Expiry checks and issuing short-lived keys are
P0-4d.
