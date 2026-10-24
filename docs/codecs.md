# Protocol codecs

`hofund-codec` translates each external protocol to and from `hofund-ir`.
One implementation per protocol is the whole matrix: a client protocol and an
upstream protocol never translate into each other directly. They both speak
the IR.

Implemented:

- `AnthropicMessages` (`anthropic-messages`)
- `OpenAiChat` (`openai-chat`)
- `OpenAiResponses` (`openai-responses`)

Not implemented: Gemini. There is no HTTP server here. The codec crate does
not open sockets, and its tests do not use the network.

## Trait

```text
Codec::decode_request / encode_request
Codec::decode_response / encode_response
Codec::decode_stream / encode_stream
Codec::stream_decoder -> StreamDecoder::{push, finish}
Codec::stream_encoder -> StreamEncoder::push
Codec::encode_error(status, message) -> JSON body
```

`stream_encoder` writes one event at a time and keeps block state between
calls. `encode_stream` is that encoder over a whole slice.

`encode_error` writes the protocol's error object. Anthropic uses
`error.type` (`invalid_request_error`, `authentication_error`,
`permission_error`, `not_found_error`, `request_too_large`, `rate_limit_error`,
`overloaded_error`, otherwise `api_error`). Chat Completions and Responses
share one shape: `error.message`, `error.type`, `error.param`, `error.code`.
Statuses 500 through 599 are `server_error`.

`decode_stream` feeds one buffer to a fresh decoder and then finishes it.
`StreamDecoder::push` accepts any chunk boundary, including a split in the
middle of an SSE line. Malformed input returns `CodecError`. Library code does
not panic on it.

Encode writes compact JSON. SSE records use `\n` and end with a blank line.
`id` and `retry` fields are ignored. Lines that start with `:` are comments.

## Extension bag

Unknown JSON fields are kept on `Extensions` and written back on encode.
Keys the codec already wrote win, so a bag cannot overwrite `model`, `messages`,
or another structural field. A colliding bag key is omitted on the wire and
does not come back.

Top-level request fields this version does not model (`metadata`,
`stream_options`, a thinking budget, and anything else unrecognized) live on
`Request.extensions`. `GenerationParams.extensions` is written on encode and
read back onto the request bag. After a protocol round trip the parameter bag
is empty.

Callers that encode with a different protocol should call
`Request::strip_extensions` (and the response and event equivalents) first.
Otherwise foreign fields are copied onto the other protocol's JSON.

Same-protocol equality is semantic. `decode(encode(decode(bytes)))` equals
`decode(bytes)`. The bytes are not required to match: a string `content` may
come back as a one-element array, and a string `stop` comes back as an array.

## Messages

The IR keeps every message, including system and developer messages, in order.

Anthropic Messages has one system channel. Decode turns a top-level `system`
string or block array into a leading `Role::System` message. Encode hoists
every `Role::System` and `Role::Developer` message into that channel, in
encounter order, and removes them from `messages`. One plain text message with
an empty bag is a string. Anything else is an array of blocks, and message-level
fields on that channel are folded onto the first block. Developer and system
become the same role. Several system messages become one message.

Chat Completions keeps `system` and `developer` in the message array.

Tool results:

- Anthropic leaves `tool_result` blocks on the user message that carried them.
  `Role::Tool` is encoded as a user message of `tool_result` blocks, and
  consecutive tool messages are merged into one user message.
- Chat Completions uses `role: "tool"` and `tool_call_id`. Decode builds one
  `Role::Tool` message whose body is a single tool-result part. Encode splits
  tool-result parts out of a user message. `is_error` has no Chat Completions
  field and is dropped (it comes back `false`).

A legacy `function_call` object on a Chat Completions message is not turned
into a tool call. It stays in the message bag and is written back.

Only `type: "function"` tools are canonical. Any other tool type is
`CodecError::Unsupported`.

## Tool arguments

`ToolArguments::Json` keeps object key order. Whitespace is not significant.
`ToolArguments::from_text` parses JSON and otherwise keeps the original text.

Anthropic `input` must be JSON. Empty raw text is encoded as `{}`. Raw text
that is not JSON is `CodecError::Unsupported`. Chat Completions always writes
`arguments` with `to_text()`, which is compact JSON for a parsed value.

Streaming argument deltas are concatenated as text. They are parsed only when
`assemble` folds the turn. A fragment may split a JSON token. Invalid JSON
stays raw. `assemble` does not return an error.

## Tool choice

| IR | Anthropic Messages | Chat Completions | Responses |
| --- | --- | --- | --- |
| `Auto` | `{"type":"auto"}` | `"auto"` | `"auto"` |
| `None` | `{"type":"none"}` | `"none"` | `"none"` |
| `Required` | `{"type":"any"}` | `"required"` | `"required"` |
| `Named { name }` | `{"type":"tool","name"}` | `{"type":"function","function":{"name"}}` | `{"type":"function","name"}` |

Responses `tool_choice` of type `allowed_tools` filters the tool list down to
the named functions and keeps the mode (`auto` or `required`). The wrapper
object is not written back. Any other tool type, and any other `tool_choice`
type, is `CodecError::Unsupported`.

Each codec writes its own spelling, so a same-protocol round trip does not
have to remember whether the wire said `any` or `required`. Extra fields such
as `disable_parallel_tool_use` live on the choice bag. Chat Completions writes
`auto`, `none`, and `required` as strings when the bag is empty, and as
objects when it is not, so those fields survive.

## Generation parameters

| IR | Anthropic Messages | Chat Completions | Responses |
| --- | --- | --- | --- |
| `max_tokens` | `max_tokens` (required on encode) | `max_completion_tokens` | `max_output_tokens` |
| `temperature`, `top_p` | same names | same names | same names |
| `top_k` | `top_k` | omitted | omitted |
| `stop` | `stop_sequences` | `stop` (always an array) | omitted |
| `seed`, penalties | omitted | same names | omitted |
| `stream: true` | `stream: true` | `stream: true` | `stream: true` |
| `stream: false` | omitted | omitted | omitted |

Anthropic encode returns `MissingField("max_tokens")` when the IR has no cap.
It does not invent one. Decode of a request may omit the cap.

Chat Completions decode reads `max_completion_tokens` and `max_tokens`. When
both are set, `max_completion_tokens` wins and the other value is dropped.
Encode writes only `max_completion_tokens`. `total_tokens` is written only
when the IR has it. The codec does not synthesize a sum.

## Images, thinking, and text

Images are `ImageSource::Url` or `ImageSource::Base64 { media_type, data }`.
A `data:<media>;base64,<data>` URL becomes base64. Any other URL, including a
`data:` URL that is not base64, stays a URL. `ImageDetail` is written only by
Chat Completions. Anthropic encode drops it.

Anthropic thinking is a `thinking` block (`text` + optional `signature`) or a
`redacted_thinking` block (`data`). An empty signature is omitted and decodes
as absent. When a thinking part is both redacted and readable, encode writes
`redacted_thinking` and the readable text is not on the wire.

Chat Completions stores readable thinking in one `reasoning_content` string.
Several readable parts are concatenated, in order, with no separator. Signatures
and redacted blobs are omitted. Decode order for an assistant message is
reasoning, then content, then tool calls, because the protocol has no block
order. A same-protocol round trip is stable because decode already used that
order. A hand-built message with text before thinking is reordered.

Chat Completions joins assistant text parts into one string when every
remaining part is text. Boundaries between those parts are lost. Mixed content
(an image or an unknown part beside text) stays an array, in order. One plain
text part with an empty bag is a string; otherwise user, system, and developer
content is an array. Unknown part types are written back as `{ "type": <kind> }`
plus their bag.

## Stop reasons

| IR | Anthropic `stop_reason` | Chat `finish_reason` | Responses `status` |
| --- | --- | --- | --- |
| `EndTurn` | `end_turn` | `stop` | `completed` |
| `MaxTokens` | `max_tokens` | `length` | `incomplete`, reason `max_output_tokens` |
| `StopSequence` | `stop_sequence` | `stop` (sequence text is dropped) | `incomplete`, reason `stop_sequence` (sequence text is dropped) |
| `ToolUse` | `tool_use` | `tool_calls` | `completed` when the turn contains a tool call |
| `ContentFilter` | `content_filter` | `content_filter` | `incomplete`, reason `content_filter` |
| `Refusal` | `refusal` | `refusal` | `incomplete`, reason `refusal` |
| `Other("failed")`, `Other("cancelled")`, `Other("incomplete")`, `Other("in_progress")`, `Other("queued")` | that token | that token | that status |
| `Other(token)` | `token` | `token` | `incomplete`, reason `token` |

`pause_turn` is `Other("pause_turn")` and is written back on Anthropic.
`function_call` as a finish reason decodes as `ToolUse` and is written back as
`tool_calls`. `StopSequence` sent through Chat Completions comes back as
`EndTurn` with no sequence. Anthropic also keeps `stop_sequence` when it has one.

More than one Chat Completions choice, or a choice whose index is not 0, is
`CodecError::Unsupported`. An empty `choices` array plus `usage` is valid.
The `object` field is structural and is not stored. `created` and
`system_fingerprint` stay in the response bag.

## Usage

Counters are optional so a stream event can carry a subset.
`Usage::merge_from` overwrites counters the patch sets.

| IR | Anthropic | Chat Completions | Responses |
| --- | --- | --- | --- |
| `input_tokens` | `input_tokens` | `prompt_tokens` | `input_tokens` |
| `output_tokens` | `output_tokens` | `completion_tokens` | `output_tokens` |
| `total_tokens` | omitted | `total_tokens` | `total_tokens` |
| `cache_read_tokens` | `cache_read_input_tokens` | `prompt_tokens_details.cached_tokens` | `input_tokens_details.cached_tokens` |
| `cache_write_tokens` | `cache_creation_input_tokens` | omitted | `input_tokens_details.cache_write_tokens` |
| `reasoning_tokens` | omitted | `completion_tokens_details.reasoning_tokens` | `output_tokens_details.reasoning_tokens` |

Responses keeps protocol totals beside the cache counters. It does not subtract
cached tokens from `input_tokens`. A null `error` is dropped. A non-null
`error` stays in the response bag. The `object` field is discarded.

## Streams

Anthropic frames are `event: <name>` plus `data: <json>`. There is no
`[DONE]`. Names: `message_start`, `content_block_start`,
`content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`,
`ping`. Delta types are `text_delta`, `thinking_delta`, `signature_delta`, and
`input_json_delta`. An unknown event name is preserved and written back with
that name. Encode synthesizes a `content_block_start` when a delta arrives for
an index that was not started. `message_start` writes the structural
`type`, `role`, empty `content`, and null stop fields; those are not bag
fields.

Chat Completions frames are `data: <json>` and the turn ends with
`data: [DONE]`. The decoder's index space is fixed:

- text is index 0
- reasoning is index 1
- tool call `tool_calls[].index` `n` is index `2 + n`

Empty text and empty argument deltas are skipped. A text or reasoning
`ContentBlockStart` is not written; decode synthesizes it before the first
delta. A tool start is a tool-call delta with `id`, `name`, and `arguments: ""`.
Signature deltas and redacted thinking starts are skipped. `ContentBlockStop`
is not written; decode emits one per open index, in ascending index order,
immediately before `MessageStop` when it sees `[DONE]`. `MessageStop` is
`data: [DONE]`, and its bag is dropped. A same-protocol round trip of decoder
output is stable. Hand-built stops in the middle of a turn are not, because
encode ignores them and decode rebuilds them at `[DONE]`.

Unknown fields on a Chat Completions chunk attach to the first event from
that chunk that encode will emit. Duplicate `id` and `model` on later chunks
do not open a second message. A chunk that is not a chat chunk is a `ping`
when its type is `ping`, and otherwise an unknown event.

Stream indexes are not shared across protocols. Translate a finished turn with
`assemble`, or translate the request and the response. Do not feed Anthropic
block indexes to the Chat Completions encoder: text must be 0, thinking must
be 1, and tool indexes must be at least 2.

Responses frames are `event: <name>` plus `data: <json>`. The decoder's index
is `output_index` (missing means 0). `response.created` and
`response.in_progress` open the turn once. A `function_call` item opens a tool
block. A `reasoning` item opens a thinking block. `output_text` opens a text
block. Empty deltas are skipped. `function_call_arguments.done` emits only the
suffix that extends arguments already streamed. When the finished arguments
diverge from that prefix, the streamed text is kept and nothing is appended.
`output_item.done` expands the item only when that index has not already
produced deltas. A terminal `response.completed`, `response.incomplete`, or
`response.failed` expands `response.output` only when no deltas have arrived;
otherwise the output array is ignored. Then open blocks close, a message delta
carries the stop reason and usage, and `message_stop` follows.

A `response.failed` frame that arrives before the turn has started, with empty
or absent output, is an unknown event named `response.failed`. The same frame
after `response.created` is the stop reason `Other("failed")`. A lone `error`
event is an unknown event and does not synthesize `message_stop`. Signature
deltas are skipped on encode.

The Responses encoder writes `response.created`, then one item per block, then
`response.completed`, `response.incomplete`, or `response.failed` from the stop
reason. A same-protocol round trip of decoder output is stable. Hand-built
events are not required to round-trip byte for byte.

## Responses items

`instructions` becomes a leading system message. Encode hoists that message
back to `instructions` only when it is the sole system message, its body is
one plain text part, and both bags are empty. Otherwise the system message
stays an input item.

`input` may be a string (one user text message) or an array. Item types:

- `message` keeps `user`, `system`, `developer`, and `assistant`. A missing
  role becomes `user`.
- `function_call` is an assistant tool call.
- `function_call_output` is a `Role::Tool` result. The output is a string or
  an array of parts. Any other JSON type is `CodecError::WrongType`.
- `reasoning` is assistant thinking. Non-empty `encrypted_content` stores the
  whole item, with `type: "reasoning"` put back, on
  `ThinkingPart.extensions["sealed_item"]`. Encode writes that object back.
  Readable text prefers `reasoning_text`, then concatenated summary text. An
  empty unsealed reasoning item is skipped. A stream copies non-empty
  `encrypted_content` onto `sealed_item` on the thinking block's open and
  close events. Assemble keeps that object on the thinking part, and the
  stream encoder writes it back. An empty seal is ignored.
- Any other item type becomes a user message with one unknown part. Encode
  nests that part inside a message content array, so the wire form changes and
  the IR stays stable.

Consecutive assistant messages are merged. Tool results are not.

Images use `image_url` as a string or as `{url, detail}`. A `data:` URL
becomes `ImageSource::Base64`. Encode writes a string URL, and base64 as
`data:<media>;base64,<data>`. `detail` is kept.

Only `type: "function"` tools are canonical. `name`, `description`, and
`parameters` are flattened. `strict` and extra keys stay on the tool bag.

Leftover request keys (`metadata`, `store`, `previous_response_id`,
`reasoning`, `parallel_tool_calls`, and anything else unrecognized) stay in
the request bag and are written back. `top_k`, `seed`, `stop`, and the penalty
fields are canonical parameters and are omitted on encode, not stored in that
bag.

`OpenAiResponses::prepare_upstream_body` is adapter policy, not a codec round
trip. It sets `store` to false when the field is absent, and it appends an
empty `function_call_output` for each `function_call` id that has no output.
