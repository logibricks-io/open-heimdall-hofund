# Protocol codecs

The model-name contract for a mapped alias and for an equal name is in
[Model names on a response](model-names.md).

`hofund-codec` translates each external protocol to and from `hofund-ir`.
One implementation per protocol is the whole matrix: a client protocol and an
upstream protocol never translate into each other directly. They both speak
the IR.

Implemented:

- `AnthropicMessages` (`anthropic-messages`)
- `OpenAiChat` (`openai-chat`)
- `OpenAiResponses` (`openai-responses`)
- `GenerativeLanguage` (`generative-language`)
- `Converse` (`bedrock-converse`)

There is no HTTP server here. The codec crate does not open sockets, and its
tests do not use the network. `GenerativeLanguage` is both an inbound client
protocol and the body an upstream adapter posts. The adapter removes `model`
and `stream` from that body: the model is a path segment, and `stream` selects
`streamGenerateContent`.

## Reasoning effort

`GenerationParams.effort` is the shared level. A codec reads its own field
and, on encode, writes that field only when the extension bag does not
already hold it. A same-protocol request therefore keeps the caller's object,
including a Messages `thinking.budget_tokens` value, which is not turned into
a level. Official lists were checked on 2026-10-25.

| Level | Chat and Responses | Messages | Generative Language |
| --- | --- | --- | --- |
| none | `none` | `thinking.type = disabled` | `thinkingBudget: 0` |
| minimal | `minimal` | `low` | `minimal` |
| low | `low` | `low` | `low` |
| medium | `medium` | `medium` | `medium` |
| high | `high` | `high` | `high` |
| xhigh | `xhigh` | `xhigh` | `high` |
| max | `max` | `max` | `high` |

Chat and Responses publish all seven labels
(<https://developers.openai.com/api/docs/guides/reasoning>).
Messages `output_config.effort` publishes `low` through `max`
(<https://platform.claude.com/docs/en/build-with-claude/effort>).
Generative Language `thinkingConfig.thinkingLevel` publishes `minimal`
through `high`
(<https://ai.google.dev/gemini-api/docs/thinking>). On decode, a positive
`thinkingBudget` maps to low through 4096, medium through 12000, high through
24000, and xhigh above that. A negative dynamic budget maps to medium, zero to
none, and `includeThoughts` alone does not request effort. The camelCase and
snake_case field spellings are both read.
`ultra` is read as `max` and written as `max`. Levels are not clamped to a
model; this version has no catalog.

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
Statuses 500 through 599 are `server_error`. Generative Language uses
`error.code` (the HTTP status), `error.message`, and `error.status`
(`INVALID_ARGUMENT` for 400 and 413, `UNAUTHENTICATED`, `PERMISSION_DENIED`,
`NOT_FOUND`, `RESOURCE_EXHAUSTED`, `UNAVAILABLE` for 503, otherwise
`INTERNAL`).

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
is empty. Generative Language is the exception: unknown `generationConfig`
keys stay on `GenerationParams.extensions` and are written back inside
`generationConfig`.

The provider drops every extension bag when the inbound protocol id differs
from the outbound codec id, before it encodes the upstream body and again on
the decoded response and stream events. Raw events the decoder does not model
are dropped on that path too, after a leading upstream failure has been turned
into an error. A same-protocol call keeps the bag. On same-protocol Responses
calls, a 400 naming an unknown member of
`internal_chat_message_metadata_passthrough` is retried once without that
member on every input item, or without the whole object when the refusal names
it directly. The provider remembers refused members and omits them before
later sends; removal preserves every other byte, repeated refusals are not
retried, and cross-protocol requests do not use this rule. The refusal parser
accepts the two upstream wordings and decodes JSON `\u` escapes before matching.
A caller that encodes with a different protocol without going through the provider should call
`Request::strip_extensions` (and the response and event equivalents) first.
Otherwise foreign fields, including caller identifiers, are copied onto the
other protocol's JSON.

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

Chat Completions accepts only `type: "function"` tools. Any other Chat tool
type is `CodecError::Unsupported`. Responses accepts more than functions;
see [Responses tools](#responses-tools).

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

| IR | Anthropic Messages | Chat Completions | Responses | Generative Language |
| --- | --- | --- | --- | --- |
| `Auto` | `{"type":"auto"}` | `"auto"` | `"auto"` | `functionCallingConfig.mode = "AUTO"` |
| `None` | `{"type":"none"}` | `"none"` | `"none"` | `"NONE"` |
| `Required` | `{"type":"any"}` | `"required"` | `"required"` | `"ANY"` |
| `Named { name }` | `{"type":"tool","name"}` | `{"type":"function","function":{"name"}}` | `{"type":"function","name"}` | `"ANY"` and `allowedFunctionNames: [name]` |

Responses `tool_choice` of type `allowed_tools` filters the tool list down to
the named functions, custom tools, and client `tool_search` entries, and
keeps the mode (`auto` or `required`). A namespaced name is matched after
flattening. The wrapper object is not written back. Any other `tool_choice`
type is `CodecError::Unsupported`.

Each codec writes its own spelling, so a same-protocol round trip does not
have to remember whether the wire said `any` or `required`. Extra fields such
as `disable_parallel_tool_use` live on the choice bag. Chat Completions writes
`auto`, `none`, and `required` as strings when the bag is empty, and as
objects when it is not, so those fields survive.

Generative Language `ANY` with exactly one `allowedFunctionNames` entry is
`Named`. `ANY` with any other list is `Required`, and the list stays on the
choice bag. `VALIDATED`, and any other mode token, is `Required` plus
`extensions.mode` set to that token, so encode writes the token back. `AUTO`
and `NONE` do not store the mode string.

## Responses tools

A Responses `tools` array may contain `function`, `custom`, `namespace`,
`web_search` (any type whose name starts with `web_search`), and
`tool_search`. Decode does not reject the request for an unknown type.

Same-protocol encode writes the original `tools` array back unchanged,
including entries that have no canonical form. `allowed_tools` still filters
that array before it is stored.

Crossing to another protocol uses the canonical tools only:

- `function` stays a function. A nested function inside `namespace` is
  offered as `namespace__name`. A name longer than 64 bytes keeps a 55-byte
  prefix, an underscore, and the first 4 bytes of SHA-256 of the pair, in hex.
- `custom` (a free-form tool, including one nested in a namespace) becomes a
  function whose arguments are one required string, `input`. The description
  says to put the whole free-form input in that string, and appends a grammar
  when `format.type` is `grammar`.
- `tool_search` with `execution: "client"` becomes a function using its
  `parameters`. Any other `tool_search` is dropped.
- `web_search` and every other type are dropped.

Dropped names are logged by the provider (`warning: dropped tool ...`) once
per name per process, and only when the upstream protocol differs. A later
request that drops the same name is silent. The drop is not a 400.

On the way back, a Responses client receives the original call. The codec
keeps a per-request map (`hofund_tool_names`) from the flattened name to the
original name, namespace, and kind. A namespaced function is written as
`function_call` with `namespace` and the original `name`. A custom tool is
written as `custom_tool_call` with `input` set to the raw string (the
`input` field of the JSON arguments, a lone JSON string, or the argument
text). The map is private, so it is not sent upstream. Chat and Messages
upstreams both go through this restore, for a unary body and for a stream.
A same-protocol reply already uses the original names, so the map is not
applied. A stream of a custom call emits
`response.custom_tool_call_input.delta` and `.done` with the unescaped input.
`encrypted_function_args` is not written.

A history item is encoded the same way. `custom_tool_call` becomes an
assistant tool call whose name is the declared flat name (`namespace__name`
when a namespace is set, otherwise the original name) and whose arguments
are `{"input": <raw>}`. `custom_tool_call_output` becomes a tool result with
that `call_id`. A namespaced `function_call` in history is sent upstream
under that same flat name, which is the name on the tool declaration.
Same-protocol Responses encode keeps `custom_tool_call`, the raw `input`,
`custom_tool_call_output`, and `namespace` plus the original function name.
The private `hofund_call_name` and `hofund_call_kind` keys are dropped when
the outbound protocol differs.

Checked 2026-10-25 against the Responses tool list and against
`internal/gateway/responses.go` (`customSchema`, namespace flattening,
`callTo`, `web_search` not offered as a function, client `tool_search`) and
`internal/gateway/custom_tool.go` (`customStream`).

## A Responses WebSocket

`GET /v1/responses`, `GET /responses`, and
`GET /backend-api/codex/responses` accept a WebSocket upgrade. The last path
is declared by the Codex catalog surface through the inbound plugin API. A
plain GET still answers 405 with `Allow: POST` on the built-in routes and keeps
its ordinary surface behavior on the Codex path; an upgrade on any other path
answers 426. Text frames are `response.create` objects and turns run one at a
time in arrival order. Binary frames close with 1003, malformed requests get a
400 error frame, and the message read limit is the same 8 MiB as the HTTP body
limit.

Each create is replayed through the ordinary Responses POST handler with the
upgrade request's URI, Host, peer data, and non-hop-by-hop headers. Socket
handshake headers are removed, JSON and SSE are requested, and client metadata
supplies the turn-state and responses-lite headers. Authentication, body
limits, routing, failover, usage rows, and request events therefore apply to
every turn again; rotating the token does not close the socket, but the old
credential fails its next turn.

The socket keeps only its own last two completed replies, plus a locally
answered `generate: false` warmup. A continuation names one of those ids in
`previous_response_id`; its held input and output items are prepended to the
new input. An unknown, evicted, failed, or other-socket id gets
`previous_response_not_found`. A warmup gets `response.created` followed by
`response.completed` with zero usage and a fresh `resp_` id, without an
upstream call.

SSE replies become one text frame per JSON event. Unary Responses become a
created event, output-item done events, and the matching terminal event.
Response headers become metadata and rate-limit frames; HTTP failures become
error frames with their status, error object, and retry/rate-limit headers.
Error bodies are capped at 1 MiB. A reply that ends without a terminal event
closes the socket with 1011.

A `response.interrupt` may name any id the turn has reported in a created,
in-progress, or completed event. A match cancels the upstream request, then
sends `response.interrupt.accepted` followed by `response.incomplete` with
reason `interrupted` and empty output. The interrupted turn is not held for
continuation, and its later writes are suppressed without closing the socket.
An unknown id or an interrupt with no turn in flight gets
`response.interrupt.failed`. A client disconnect also cancels the turn in
flight.

During graceful shutdown, new socket upgrades get 503 `hofund is restarting`.
Idle sockets close 1001 immediately; a busy socket closes 1001 after its turn.
Shutdown waits for those sockets until its deadline, then cuts any that remain.
Socket turns are tracked by this socket drain, not by the HTTP request drain.

The upstream leg still receives the whole expanded conversation on every
turn; only the client's upload to hofund is reduced. Socket history is memory
only and is lost when the socket closes or the gateway restarts. P0-1e (#481)
added the Codex-backend Responses path with this same behavior; it is no longer
an out-of-scope gap.

## Client-facing ids and Chat usage chunks

A Responses body and each `response.created` / completed skeleton use an id
that starts with `resp_`. A missing id becomes `resp_local`. An id that
already has the prefix is kept. Output item ids start with `msg_`, `rs_`,
`fc_`, or `ctc_` and then a suffix of time and a process counter, so two
items in one response, and two responses, do not share an id. A unary output
item also has `status: completed` when the caller did not set one. A sealed
reasoning item is written back as stored, unless it carries content parts
(another vendor's item), which is made over as described below. A
same-protocol response keeps an
item id the upstream already sent, on the unary body and on the stream. The
stream copies that id onto the block (`item_id`) and the encoder prefers it
over a fresh one.

A non-streaming Messages body always has an id. A missing id becomes
`msg_local`. An id without the `msg_` prefix is prefixed. The stream already
did this on `message_start`.

A Chat Completions stream remembers the id and model from the first chunk and
writes them on every later chunk. Usage is a separate chunk with
`choices: []` and is written only when the client request set
`stream_options.include_usage` to true. The finish chunk does not carry
`usage`. The Chat adapter still asks the upstream for usage, so the ledger
can record it when the client did not ask to see it. When a usage object is
written it always includes `prompt_tokens`, `completion_tokens`, and
`total_tokens`. See Usage for how a Messages stream is merged before that.

## Billing header

Claude Code may send a system text block that is only
`x-anthropic-billing-header:` plus `key=value;` fields, or that prefix at the
start of the first system text. There is no public schema for the line. It
is kept when the upstream protocol is Messages. Crossing to another protocol
removes a system or developer text part that is only the line, and cuts the
same prefix off the first remaining system or developer text. A mention in
user text, or later in a system string, stays. Checked 2026-10-25 against
`internal/gateway/billing_line.go` and `withoutBillingHeader`.

## Generation parameters

| IR | Anthropic Messages | Chat Completions | Responses | Generative Language |
| --- | --- | --- | --- | --- |
| `max_tokens` | `max_tokens` (always written; see below) | `max_completion_tokens` | `max_output_tokens` | `generationConfig.maxOutputTokens` |
| `temperature`, `top_p` | same names | same names | same names | `temperature`, `topP` |
| `top_k` | `top_k` | omitted | omitted | `topK` |
| `stop` | `stop_sequences` | `stop` (always an array) | omitted | `stopSequences` (always an array) |
| `seed` | omitted | `seed` | omitted | `seed` |
| penalties | omitted | same names | omitted | omitted |
| `stream: true` | `stream: true` | `stream: true`. The Chat adapter, not the codec, also sets `stream_options.include_usage` so the upstream returns usage. A codec round trip does not add that object. | `stream: true` | `stream: true` on the body |
| `stream: false` | omitted | omitted | omitted | omitted |

Messages encode always writes `max_tokens`. The client's cap wins. Otherwise
the router copies the route member's `max_output_tokens`, or the provider's
when the member omits it, and only when the client omitted a cap. The
`messages-default` quirk then fills 4096 if the cap is still absent. Messages
encode uses 16384 when all of those are absent
(`internal/gateway/maxoutput.go`, `defaultMaxTokens`, checked 2026-10-25).
Chat and Responses do not invent a cap. Decode of a request may omit it.
Create requires the field
(<https://platform.claude.com/docs/en/api/messages>). A body the codec cannot
encode is HTTP 400 `invalid_request_error`. The Messages provider deletes
`max_tokens` before `POST /v1/messages/count_tokens`. That endpoint rejects
the field. The permitted count fields include `model`, `messages`, `system`,
`tools`, `tool_choice`, `thinking`, `cache_control`, and `output_config`
(<https://platform.claude.com/docs/en/build-with-claude/token-counting>,
checked 2026-10-25).

Chat Completions decode reads `max_completion_tokens` and `max_tokens`. When
both are set, `max_completion_tokens` wins and the other value is dropped.
Encode writes only `max_completion_tokens`. `total_tokens` is the IR total.
When that is absent and both `input_tokens` and `output_tokens` are present,
encode writes their sum.

## Images, thinking, and text

Images are `ImageSource::Url` or `ImageSource::Base64 { media_type, data }`.
A `data:<media>;base64,<data>` URL becomes base64. Any other URL, including a
`data:` URL that is not base64, stays a URL. `ImageDetail` is written only by
Chat Completions. Anthropic encode drops it.

Anthropic thinking is a `thinking` block (`text` + optional `signature`) or a
`redacted_thinking` block (`data`). An empty signature is omitted and decodes
as absent. When a thinking part is both redacted and readable, encode writes
`redacted_thinking` and the readable text is not on the wire. Same-protocol
translation keeps client `cache_control` marks and TTLs on ordinary content,
but does not write a mark on either thinking block type and adds no marks of
its own. An invalid sequence that moves from a five-minute mark to a
one-hour mark drops the client marks instead of rejecting the translation.
A complete inline thought-signature suffix is removed from tool-call
and tool-result ids on request decode; a truncated suffix is left alone.
`tool_choice: any` is rejected when no tools are declared.

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

| IR | Anthropic `stop_reason` | Chat `finish_reason` | Responses `status` | Generative Language `finishReason` |
| --- | --- | --- | --- | --- |
| `EndTurn` | `end_turn` | `stop` | `completed` | `STOP` |
| `MaxTokens` | `max_tokens` | `length` | `incomplete`, reason `max_output_tokens` | `MAX_TOKENS` |
| `StopSequence` | `stop_sequence` | `stop` (sequence text is dropped) | `incomplete`, reason `stop_sequence` (sequence text is dropped) | `STOP` (sequence text is dropped) |
| `ToolUse` | `tool_use` | `tool_calls` | `completed` when the turn contains a tool call | `STOP` |
| `ContentFilter` | `content_filter` | `content_filter` | `incomplete`, reason `content_filter` | `SAFETY` |
| `Refusal` | `refusal` | `refusal` | `incomplete`, reason `refusal` | `REFUSAL` (decodes as `Other("REFUSAL")`) |
| `Other("failed")`, `Other("cancelled")`, `Other("incomplete")`, `Other("in_progress")`, `Other("queued")` | that token | that token | that status | that token |
| `Other(token)` | `token` | `token` | `incomplete`, reason `token` | `token` |

`pause_turn` is `Other("pause_turn")` and is written back on Anthropic.
`function_call` as a finish reason decodes as `ToolUse` and is written back as
`tool_calls`. `StopSequence` sent through Chat Completions comes back as
`EndTurn` with no sequence. Anthropic also keeps `stop_sequence` when it has one.

More than one Chat Completions choice, or a choice whose index is not 0, is
`CodecError::Unsupported`. An empty `choices` array plus `usage` is valid.
The `object` field is structural and is not stored. `created` and
`system_fingerprint` stay in the response bag.

## Usage

Counters are optional on the IR so a stream event can carry a subset.
`Usage::merge_from` overwrites counters the patch sets and leaves the others.
`Usage::merge_patch` applies that to the encoder's accumulated usage.

Messages puts input and cache tokens on `message_start` and output tokens on
`message_delta`. Anthropic encode keeps that split: `message_start.message.usage`
is the start event, and the single `message_delta` carries the delta. Responses,
Chat Completions, and Generative Language encode merge the two before writing
one usage object. A later counter replaces only the counters it sets, so an
output-only delta does not drop the input tokens from `message_start`. When
`message_delta` has no usage, the accumulated start usage is still written on
the final chunk. No usage object is written when the IR never supplied one.

When a usage object is written, the required counts are always present. A
missing input or output is 0. A missing total is the sum. Responses always
includes `input_tokens`, `output_tokens`, `total_tokens`,
`input_tokens_details` (`cached_tokens`), and
`output_tokens_details` (`reasoning_tokens`). Cache and reasoning details are
0 when the IR did not set them (checked 2026-10-25,
<https://platform.openai.com/docs/api-reference/responses/object>).
`cache_write_tokens` stays on the IR usage object and is not written on a
Responses usage object. Chat always
includes `prompt_tokens`, `completion_tokens`, and `total_tokens`; the details
objects stay optional. Generative Language always includes `promptTokenCount`,
`candidatesTokenCount`, and `totalTokenCount`; cache and thoughts counts stay
optional.

| IR | Anthropic | Chat Completions | Responses | Generative Language |
| --- | --- | --- | --- | --- |
| `input_tokens` | `input_tokens` | `prompt_tokens` | `input_tokens` | `promptTokenCount` |
| `output_tokens` | `output_tokens` | `completion_tokens` | `output_tokens` | `candidatesTokenCount` |
| `total_tokens` | omitted | `total_tokens` | `total_tokens` | `totalTokenCount` |
| `cache_read_tokens` | `cache_read_input_tokens` | `prompt_tokens_details.cached_tokens` | `input_tokens_details.cached_tokens` | `cachedContentTokenCount` |
| `cache_write_tokens` | `cache_creation_input_tokens` | omitted | omitted | omitted |
| `reasoning_tokens` | omitted | `completion_tokens_details.reasoning_tokens` | `output_tokens_details.reasoning_tokens` | `thoughtsTokenCount` |

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
fields. `message.id` is always present and always starts with `msg_`. An id
that already has that prefix is kept; any other id is prefixed; a missing id
is `msg_local`. `message.usage` is always present. When the IR has no usage
yet, the counters are zero. `content_block_stop` is written before
`message_delta`. Every `message_delta` in the IR is held and emitted once,
after open blocks have stopped and immediately before `message_stop`. That
one event carries usage. When the IR never supplied usage, the delta's
`output_tokens` is zero.

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

The Responses encoder writes `response.created`, then one item per block.
`output_index` is assigned in the order items are opened, starting at 0, and
`response.completed.output` is that same order. A Chat tool call whose IR
index is 2 or higher is still `output_index` 0 when it is the only item.
Closing a text block writes `response.output_text.done` and
`response.content_part.done` with the accumulated text, then
`response.output_item.done` whose item has `id` and `content`. A function
call's done item has `id`, `call_id`, `name`, and the accumulated `arguments`.
A restored custom call uses `custom_tool_call` and
`response.custom_tool_call_input.delta`.
A reasoning done item has `id` and the accumulated text, or the sealed item
when one was stored. `response.completed`, `response.incomplete`, or
`response.failed` then carries that full `output` array. Every event includes
`sequence_number`, starting at 0. The decoder ignores `sequence_number`.
`response.output_text.done` contributes text only when that index has not
already produced a delta. `response.content_part.done` is ignored, because
`output_item.done` still expands an index that has no deltas. A same-protocol
round trip of decoder output is stable. Hand-built events are not required to
round-trip byte for byte.

## A Responses request that names the response it follows

A `/v1/responses` request may send only its new `input` and name
`previous_response_id`. The gateway expands a known id before decoding and
routing: the stored input and output items precede the new items, and the id is
removed. A translated upstream therefore receives the whole conversation. A
string input is one user message item. A non-string, non-array input is left
unchanged and is not kept.

Successful unary and SSE replies are captured as client-bound bytes pass, up to
64 MiB, and committed only after a normal end. Failed, cancelled, non-200,
interrupted, and over-cap replies are not kept. `store: false` also keeps
nothing, although it does not prevent resolving an earlier id.

History is in memory, scoped to the authenticated gateway caller, least
recently used first out above 256 MiB of new-turn bytes, and expired after six
hours. One turn larger than half the cap is not kept. Shared earlier items are
not counted again. History is lost on restart, and the whole local request body
is still read before any upstream delta is selected.

An unknown id on a translating route or subscription account returns 400
`previous_response_not_found` before an upstream call. A same-protocol
Responses relay passes the id upstream. With explicit `store: true`, the same
API-key route on a response-keeping host may receive only the new items and its
own upstream id; a 400 or 404 refusal naming the previous response causes one
whole-body retry. An omitted `store` keeps hofund's upstream `store: false`
privacy default and never selects this delta path.

Request masking is not currently implemented (A14 is therefore N/A). When that
feature is added, it must run after this expansion on every turn so stored text
is masked identically to text sent in the current request.

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

  A reasoning item the gateway hands a client (a reply, not a request) has
  one `summary_text` summary and no content: `{id, type, status, summary}`
  with an `rs_` id. A stream announces it as `{id, type, status:
  "in_progress", summary: []}`, never with a seal, then sends
  `response.reasoning_summary_part.added` before the first delta,
  `response.reasoning_summary_text.delta` per delta, and
  `reasoning_summary_text.done` and `reasoning_summary_part.done` before the
  item's done event. A sealed item with a non-empty `content` array is
  another vendor's: it is made into the same shape, its `reasoning_text`
  (else its summaries, joined) as the one summary, its id kept (a fresh
  `rs_` id when it has none, and on a stream the id its events carried), its
  content and seal dropped. A client may send the item back to an upstream
  that refuses a reasoning item with content and cannot open another
  vendor's seal. A Responses reply relayed raw goes through the same rule:
  each `data:` line whose event `item`, `response.output` or response-object
  `output` holds such an item is rewritten, and every other line and byte,
  sequence numbers included, stays (`reasoning_tidy.rs`). A request item
  still carries its text as `reasoning_text` content.
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

## Generative Language

The public REST methods put the model in the URL:
`POST /v1beta/models/{model}:generateContent` and
`POST /v1beta/models/{model}:streamGenerateContent`. This codec also reads
and writes `model` and `stream` on the JSON body. The gateway copies the path
model onto the body (the path wins) and sets `stream` from the method name
before decode. `streamGenerateContent` is a stream even when the query has
no `alt=sse`. An empty model is omitted on encode. There is no upstream
adapter for this protocol in this version, so encode of `model` and `stream`
exists for the gateway injection and for a same-protocol round trip.

`systemInstruction` becomes a leading `Role::System` message. A string is one
text part and is written back as `{parts: [...]}`. Encode hoists every
`Role::System` and `Role::Developer` message into that one channel, in
encounter order, and removes them from `contents`. Developer and system share
the channel.

`contents[].role` is `user` or `model` (`Role::Assistant`). `Role::Tool`
encodes as a `user` content of `functionResponse` parts, and consecutive tool
messages merge into one content. Decode always comes back as `Role::User`.

Parts, in priority order: `functionCall`, `functionResponse`, `inlineData`,
`fileData`, then `thought: true` or a lone `thoughtSignature`, then `text`,
then an unknown part whose bag is the leftover object. `functionCall.args`
must be a JSON object. A missing object becomes `{}`. Any other JSON type is
`CodecError::Unsupported`. An empty call id is omitted on encode.

`functionResponse.response` is a JSON object. Decode stores that object as
compact JSON text on the tool-result part. Encode uses a single text part
when it parses as a JSON object, and wraps other text as `{"result": text}`.
The function name lives on the tool-result bag (`name`). `is_error` has no
wire field and comes back `false`. Non-text tool-result content is
`CodecError::Unsupported`.

`inlineData` is `ImageSource::Base64`. `fileData.fileUri` is
`ImageSource::Url`, and `mimeType` stays on the image bag so encode can put
it back inside `fileData`. `ImageDetail` is dropped. A redacted thinking blob
has no wire field. Encode writes readable thinking text and a non-empty
signature, and drops `redacted` and `data`.

`thought: true` is a thinking part. `thoughtSignature` on that part is
`ThinkingPart.signature`. `thoughtSignature` on a function call or on plain
text has no IR field, so it stays on that part's extension bag and is written
back beside `functionCall` or `text`. `thought: true` beside a function call
is kept on the call bag. It is not a second thinking part.

Function declarations live under `tools[].functionDeclarations` (`name`,
`description`, `parameters`). Other keys on a declaration stay on the tool
bag. A tool object that has no declarations is kept as `toolExtras` and
written back as another tools entry. Extra keys on the first declarations
wrapper stay in `declarationWrapper` and merge back onto that object. Further
mixed wrappers are split into `toolExtras` (the declarations are still kept).

Only candidate index 0 is canonical. When no candidate has index 0, the first
candidate is used and its index is written back as 0. Further candidates are
stored on `extensions.extraCandidates` and written back after the canonical
one. `STOP` with a function call in the message is `ToolUse`. `STOP` without
one is `EndTurn`. An `EndTurn` that also contains a function call therefore
comes back as `ToolUse`. `responseId` is the response id. `modelVersion` is
the model. A body that only has `model` is accepted and written back as
`modelVersion`. Usage counters are not subtracted from each other.
`cache_write_tokens` is omitted. `promptFeedback` and other leftover response
fields stay on the response bag. Candidate fields other than `content`,
`finishReason`, and `index` stay on the message bag and are written back on
the candidate. Unknown fields inside `content` move up onto that candidate.

Stream frames are `data: <json>` and a blank line. There is no event name.
`data: [DONE]` closes open text and thinking blocks and ends the turn. A
later `[DONE]` does not emit a second stop. The decoder's index space is
fixed, and it is not shared with other protocols:

- text is index 0
- thinking is index 1
- each function call takes the next index from 2 upward, in encounter order

A function call in one chunk emits a start, one arguments delta with the
compact JSON object, and a stop. The encoder writes that call when the block
stops, or flushes any call that is still open before a message delta. A
thinking part with both text and a signature becomes a thinking delta and
then a signature delta, so a round trip of decoder output splits them into
two frames. Empty text is skipped. `Ping` is omitted. A JSON value that is
not a generate chunk is an unknown event named `data` and is written back.
Images, function responses, and unknown parts in a stream are unknown events
named `part`. Same-protocol equality is `decode(encode(decode(bytes)))`.
Hand-built events that delay a function-call stop past a later text delta are
not required to round-trip, because the encoder holds the call until the stop.

## Converse

Bedrock Converse puts the model in the URL:
`POST /model/{modelId}/converse` and
`POST /model/{modelId}/converse-stream`. This codec does not write `model`
or `stream` into the JSON body. The provider percent-encodes the model id
for SigV4 (`:` becomes `%3A`) and chooses the method from `request.stream`.

`system` is an array of text blocks. `Role::System` and `Role::Developer`
are both written there, in order. A non-text system part is an error.
`messages` alternate `user` and `assistant`. Adjacent messages with the same
role are merged. `Role::Tool` is a user message of `toolResult` blocks. The
first message has to be `user`.

`inferenceConfig` carries `maxTokens`, `temperature`, `topP`, and
`stopSequences`. `top_k` is written to `additionalModelRequestFields`, which
is where the Converse examples put a field `inferenceConfig` does not have.
`seed`, `presence_penalty`, `frequency_penalty`, and reasoning effort are
rejected. Effort is not given an invented thinking budget. Image `detail` is
not sent. An image URL is an error, because Converse image `source` is
`bytes`. Formats are `png`, `jpeg`, `gif`, and `webp`.

`toolConfig.tools[].toolSpec.inputSchema.json` is the tool's JSON Schema.
`toolChoice` is `auto`, `any` (required), or `tool.name`. An explicit choice
of none omits `toolConfig`. A tool input that is not a JSON object is an
error.

`stopReason` maps `end_turn`, `max_tokens`, `stop_sequence`, `tool_use`, and
`content_filtered` or `guardrail_intervened`. Anything else stays
`StopReason::Other`. `additionalModelResponseFields.stop_sequence` is the
matched sequence. Usage reads `inputTokens`, `outputTokens`, `totalTokens`,
`cacheReadInputTokens`, and `cacheWriteInputTokens`.

The stream is an AWS event-stream, not SSE. `contentBlockDelta` of `text`
opens a text block on first use. `messageStop` is held until `metadata`
carries usage, then one message delta carries both the stop and the usage.
An exception frame is an error. A truncated frame is an error.
