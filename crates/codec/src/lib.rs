//! Protocol codecs that encode and decode between external LLM APIs and the canonical
//! IR. The design is hub-and-spoke: each protocol is implemented once against `ir`,
//! so N client protocols and M upstream protocols compose without a direct N-by-M
//! translator matrix. Planned protocols are Anthropic Messages, OpenAI Chat
//! Completions, and OpenAI Responses, with Gemini later. Protocol names belong in
//! this crate; vendor capability gaps belong in `quirks`.

// Referenced so this crate's manifest dependency is compiled, not merely declared.
use hofund_ir as _;
