# Model names on a response

A client asks for a model. Hofund may send a different name upstream when the
route has an alias or a wire mapping. The response follows one rule on both
the typed path and the raw path.

- When the name sent upstream differs from the name the client asked for,
  every response carries the client-requested model in that protocol's model
  field. The raw path splices only the bytes of that field (`model`,
  `message.model`, `response.model`, or `modelVersion`). The typed path
  writes the same client name onto the decoded response.
- When the names are equal, there is no mapping. The response carries whatever
  model the upstream returned, including a dated snapshot. The raw body is
  copied through with no rewrite parse. The typed path keeps the model the
  member decoded.

The splice is keyed by the field path, never by the string value. A tool
argument whose text equals the wire name stays byte for byte. A leading BOM
is stripped and the rest uses that path. An event larger than 1 MiB is
scanned once in the held prefix. A complete model string there is spliced and
the rest of the event is streamed unchanged. If that string is not in the
prefix, or the event is invalid JSON, has trailing garbage, holds two JSON
values, or is flushed unfinished after an upstream error, the bytes are
forwarded unchanged. Each of those cases increments
`hofund_model_rewrite_skipped_total{reason}` and is logged at debug. A model
value that is an array or an object is left alone; only a JSON string at the
field path is rewritten. The SSE sentinel `data: [DONE]` is not a skip.
