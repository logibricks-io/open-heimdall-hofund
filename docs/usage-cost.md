# Usage cost and display currency

Each gateway request stores the list price used and the computed USD cost on its
usage row. Summaries sum stored costs only; rows without a known price are counted
as **unpriced** and do not add zero to totals.

## Price precedence

For a provider id and wire model name, hofund resolves USD per million tokens in
this order (same shape as the reference `ModelPrices` map):

1. `[usage.prices."<provider>/<model>"]`
2. `[usage.prices."<provider>/*"]`
3. `[usage.prices."*/<model>"]` (any provider)
4. The provider's live model list (`input`, `output`, and `cache_read` when listed)
5. The models.dev catalog (`provider/model`, then bare model id)

Cache-write tokens use five-minute and one-hour rates when present; reasoning
tokens are billed as output.

## Configuration

```toml
[usage]
# Display currency for GET /usage and `hofund usage` totals (default usd).
currency = "usd"
# Fixed USD→CNY rate for display when currency = "cny". When set, no live FX fetch runs.
cny_per_usd = 7.2

[usage.prices."primary/demo"]
input = 1.0
output = 2.0
cache_read = 0.25
cache_write = 1.25
# Optional one-hour cache-write rate; Claude models default to 2× input when omitted.
cache_write_1h = 2.0
```

Catalog overrides under `[[catalog.overrides]]` still apply to models.dev lookup.
Provider `free = true` and explicit zero prices count as priced at zero cost.
