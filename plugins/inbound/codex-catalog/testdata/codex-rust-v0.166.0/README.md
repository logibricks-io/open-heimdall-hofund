# Codex models-manager snapshot

Bundled catalog text for hofund's Codex model catalog plugin.

| Field | Value |
| --- | --- |
| Source | openai/codex `codex-rs/models-manager/models.json` and `prompt.md` |
| Tag | `rust-v0.166.0` |
| License | Apache-2.0 |

The plugin embeds a reduced copy (`slug` and `instructions_template` only) under
`src/official/`. Tests assert the embed stays aligned with the files here.
