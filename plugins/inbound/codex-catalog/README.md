# Codex catalog inbound plugin

Serves Codex's model catalog and ChatGPT backend on the gateway address.

## Model instructions (D-34)

Each catalog entry's `base_instructions` come from official Codex text only. Hofund
does not inject its own prompt.

Resolution order for a gateway model id:

1. **`models_cache.json`** under `CODEX_HOME` (default `~/.codex`), when readable.
   A matching entry copies `base_instructions` and `model_messages` as stored.
2. **Bundled catalog** at `src/official/codex_instructions.json`, built from
   `testdata/codex-rust-v0.166.0/models.json` (`instructions_template` per slug).
   Slugs match Codex 0.166: longest bundled slug whose name equals the model's
   last `/` segment or is a prefix ending at `-`.
3. **`prompt.md`** (`src/official/prompt.rs`, an ASCII string literal decoded
   byte for byte from `testdata/codex-rust-v0.166.0/prompt.json`) for unknown models and `group/…`
   routing routes.

The canonical snapshot lives under `testdata/codex-rust-v0.166.0/` with source
tag and license. Tests `d34_7` fail when embeds drift from that tree.

## Catalog size cap

Codex before **0.166.0** reads at most 1 MiB of `models.json`; older clients
get `x-hofund-left-out` when entries are trimmed from the end. From **0.166.0**
on, `?client_version=` disables the cap on both `GET /v1/codex/models` and
`GET /backend-api/codex/models`.
