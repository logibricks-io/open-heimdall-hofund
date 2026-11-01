# Provider API keys

A provider can have several upstream API keys. These are credentials used to
call the vendor, not the named gateway keys that callers use to authenticate
to hofund.

## Configuration

`api_key` remains the first key. Add later keys in candidate order with
`[[providers.keys]]`:

```toml
[[providers]]
id = "relay"
protocol = "openai-chat"
base_url = "https://relay.example/v1"
api_key = "env:PERSONAL_KEY"
key_name = "Personal"
key_weight = 1

[[providers.keys]]
key = "env:TEAM_KEY"
name = "Team"
on = true
protocol = "openai-chat"
weight = 3

[[routes]]
model = "demo"
routing = "weight"
upstreams = ["relay"]
```

Each additional key accepts only `key`, `name`, `on`, `protocol`, and `weight`.
The first key uses `key_name`, `key_protocol`, and `key_weight` beside
`api_key`. `on` defaults to true, an absent or empty `protocol` allows the
provider's protocol, and `weight` defaults to 1 and may be 1 through 1000.
Protocol values are the canonical codec ids. Unknown fields, protocols, and
weights outside that range are configuration errors. Empty keys and duplicate
resolved credentials are candidates only once.

Credential values use the same `env:NAME` form as `api_key`. Key edits preserve
that form rather than writing the resolved secret. Debug output, control
responses, and CLI listings never contain a full key. A key's public id is the
first ten hexadecimal characters of its resolved credential's SHA-256 digest;
listings show a suffix mask.

## Routing and rests

Every enabled key that supports the provider's protocol becomes an adjacent
route member. The first key comes first, followed by additional keys in file
order. A failure that rests a key does not rest its siblings, so the next key
of the same provider is tried before the next provider. Rested keys go last.

With one enabled key, the existing provider member id is unchanged. With
several enabled keys, each member id carries the key fingerprint in the
existing `preset:<provider>:<key-id>` form. Rests in `rests.json` follow those
ids, including per-key credit ladders. Strategy measurements and rounds also
use member ids, so order, rotate, usage, pace, weight, free, fast, smart, and
cheap routing all operate on keys. Weighted rounds use each key's weight.
Conversation affinity can stay with the member that answered without a
key-specific affinity implementation.

## Commands

```text
hofund provider keys <provider>
hofund provider key-add <provider> [--protocol P] <keys...>
hofund provider key-remove <provider> <key-id>...
hofund provider key-on <provider> <key-id>
hofund provider key-off <provider> <key-id>
hofund provider key-use <provider> <key-id>
hofund provider key-rename <provider> <key-id> <name>
hofund provider key-replace <provider> <key-id> <key>
hofund provider key-weight <provider> <key-id> <0-1000>
hofund provider key-protocol <provider> <key-id> <protocol|any>
```

`key-add` splits pasted keys on lines, whitespace, commas, or semicolons,
removes surrounding quotes, and reports added and already-present counts.
`key-add --protocol` restricts the added keys to one codec. `key-remove` with
several fingerprints removes them at once and reports how many went; when the
first key is among them the next enabled key takes its place. `key-weight`
sets a key's share under weighted routing (0 resets it to the default, 1).
`key-protocol` restricts a key to one codec, and `any` clears the restriction.
`key-use` enables the selected key and makes it first. Removing or disabling
the last enabled key is refused. `key-replace` preserves name, order, protocol,
weight, and enabled state, returns the old and new fingerprints, and moves
rests to the new member id. The control API exposes the same operations at
`GET` and `POST /v1/hofund/providers/{id}/keys`.

## Follow-ups

Per-key concurrency limits and remote allowance/limit readings are not yet
implemented. Per-key plan selection is not part of the configuration schema.
Limiting how many accounts or keys take requests at once, with the remainder
held as reserve keys, is a separate later task.
