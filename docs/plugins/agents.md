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
