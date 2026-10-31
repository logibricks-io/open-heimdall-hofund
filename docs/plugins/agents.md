# Agent descriptors

A data-only agent plugin declares its metadata in `agent.toml`. The registry validates the descriptor and compiles it into an `AgentDescription`; the CLI and agent engine consume that metadata without agent-specific rules.

## Ids

Agent ids are unique within the agent kind and may equal a provider, subscription, or inbound plugin id. Existing agent config and commands continue to use the bare id. Reserved infrastructure ids are rejected in every kind. Code plugins with a `HostContext` isolate host storage by kind and id; data-only agents do not use a `HostContext`.

## Model spelling

The optional top-level `model_prefix` field declares the prefix the agent puts before a gateway route name. Its value must be non-empty and end in `/`. For example, OpenCode declares `model_prefix = "hofund/"`, so `hofund/route-a` selects `route-a`.

When the field is present, route validation strips that exact prefix before comparing the model with configured route names. A bare route name or a different prefix does not match. When it is omitted, the complete model string must equal a route name.

## File locations and `create`

Multiple `[[file]]` tables with the same `id` are ordered location candidates for one logical file. The optional `create` field defaults to `true`. When no candidate exists, the engine chooses the first candidate with `create = true`; an existing candidate is chosen regardless of this field.

`create` is valid only when the same file id has more than one location. The parser rejects both `create = true` and `create = false` on a single-location file. Omitting the field gives that location the default creation behavior; an explicit `false` would otherwise have no effect because the sole location is also the fallback.

## Keys in the OS credential store

Some agents read their API key from the OS credential store rather than a config file. The optional `[credential]` table declares that item:

```toml
[credential]
file = "settings"            # managed file the item belongs to
url = "{v1}"                 # item URL (server); a template over {origin} and {v1}
account = "Bearer"           # literal account name
label = "app-label"          # literal label, for stores that keep one
windows_target = "app:url={v1}"  # Windows generic-credential target; a template
```

The table needs `route = "gateway"`. `url` and `windows_target` may not name `{token}`: the key is the item's secret and never appears in a file, a plan, an error or a log. `account` and `label` are literal text.

The engine reaches the store through `Session::with_credential_store`; tests pass an in-memory fake (`hofund_kit_agent::MemoryCredentialStore`) and never touch the real store. Without a store, connecting the agent fails before any file is written, and status reports it disconnected.

- Connect writes the key before the config files, so the agent never sees the gateway URL without a key. A plan lists the item as `credential store: <url>`; when the item already holds the key nothing is written and nothing is listed.
- A record beside the managed file (`<file>.hofund.credential`) names the item and holds a SHA-256 digest of the key, never the key. When the gateway URL changes, the old item is removed if it still holds hofund's key.
- Token refresh rewrites the item only when connect recorded one.
- Status reports drift when the item is missing or holds another key.
- Disconnect removes the item only while it holds hofund's key; an item the user replaced is left in place with a warning. A forced disconnect removes it regardless. When the store cannot be reached, the files are still restored, the item is left with a warning, and the record stays so a later disconnect retries.

### Native stores

The daemon and the desktop app use `hofund_plugin_host::native_credential_store()` (through `hofund_daemon::agent_session()`). It talks to the OS store directly and never starts a child process:

| OS | Item | Notes |
|---|---|---|
| macOS | Internet password in the default keychain: server = item URL, account = item account. Lookups leave protocol, port, path and authentication type as wildcards, so an item the agent wrote itself is found. | Items are created with the keychain's default access list (hofund only), so the agent may ask once before it reads the key. |
| Linux | Secret Service item in the default collection, found by the attributes `url` and `username` (the account), labelled with the item label; content type `text/plain; charset=utf8`. | A locked collection is unlocked through the desktop prompt, which is dismissed after 60 s. When there is no default collection, write creates one. |
| Windows | Generic credential whose target name is the item's Windows target, user name = account, persistence `Local`, UTF-8 blob. | |

Every call runs on a worker thread and gives up after 90 s, prompts included. Store errors name the store and never contain the key.
