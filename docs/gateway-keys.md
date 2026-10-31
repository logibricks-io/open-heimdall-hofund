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

Only a SHA-256 digest and a masked display form of each secret are stored.
Plaintext secrets are returned once by add and rotate; they cannot be read back
later. Constant-time digest comparison covers every stored key before a request
is accepted or refused. Backup exports contain digests, not plaintext secrets.

On first load, the existing single token becomes the default sharing key. Its
secret does not change, so connected agents keep working. `hofund token show`
still prints that token, and `hofund token rotate` rotates the default key and
its legacy token-file mirror. The default key cannot be made read-only. Removing
it leaves a tombstone so migration does not resurrect it.

## CLI

```sh
hofund gateway-key add "Remote laptop"       # prints the new secret once
hofund gateway-key add Family --key -       # reads an existing secret from stdin
hofund gateway-key list
hofund gateway-key list --json
hofund gateway-key rotate <id>
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
```

The list columns are `ID NAME STATUS MODELS ACCOUNTS ACCESS LIMIT UNTIL`.
`models`, `accounts`, `quotas`, `access`, and `issuer` also work without a value
to show the current setting. Use `access <id> read-only` to store the read-only
flag; its route enforcement is a later phase.

Authentication accepts `Authorization: Bearer <key>`, `x-api-key`,
`x-goog-api-key`, or `?key=<key>`. One enabled match is enough. Missing, unknown,
disabled, and removed keys receive 401. Existing loopback behavior is unchanged:
hofund still requires a key on loopback.

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

## Control API and backup

The authenticated control plane provides:

- `GET /v1/hofund/gateway-keys`
- `POST /v1/hofund/gateway-keys` with an `add`, `rotate`, `rename`, `on`, `off`,
  `remove`, `move`, `models`, `accounts`, `quotas`, `access`, or `issuer` action
- `GET /v1/hofund/gateway-keys/export` for hashed backup records
- `POST /v1/hofund/gateway-keys/restore` to replace the store from those records

`hofund-control-client` has typed methods for all four endpoints. Secret-bearing
requests and responses redact secrets from `Debug` output.

## Later phases

Limit records, read-only flags, expiry, issuer ids, and issuer permission flags
are stored now so older files remain forward-compatible. Spending-limit
enforcement and its CLI are P0-4b. Read-only route enforcement and `/me` are
P0-4c. Expiry checks and issuing short-lived keys are P0-4d.
