# Subscription conformance matrix

This matrix exercises the public subscription factory, subscription, and upstream provider APIs against recorded fixtures only. It uses no vendor accounts or live network services. `pass` means the cell executes and returns the expected text or tool-call shape; `N/A` means the plugin does not serve that combination and the test does not call `execute`.

The executable declaration is `plugins/registry/tests/subscription_conformance/matrix.rs`. Each executed cell uses fresh host state. Streaming replies are collected to completion, including same-protocol raw streams.

<!-- BEGIN GENERATED MATRIX -->
| Vendor | Inbound shape | Unary text | Unary tools | Stream text | Stream tools |
|---|---|---|---|---|---|
| Claude | OpenAI Chat | N/A | N/A | N/A | N/A |
| Claude | OpenAI Responses | N/A | N/A | N/A | N/A |
| Claude | Anthropic Messages | N/A | N/A | pass | pass |
| Claude | Generative Language | N/A | N/A | N/A | N/A |
| Cursor | OpenAI Chat | N/A | N/A | pass | pass |
| Cursor | OpenAI Responses | N/A | N/A | N/A | N/A |
| Cursor | Anthropic Messages | N/A | N/A | pass | pass |
| Cursor | Generative Language | N/A | N/A | N/A | N/A |
| Devin | OpenAI Chat | pass | pass | pass | pass |
| Devin | OpenAI Responses | N/A | N/A | N/A | N/A |
| Devin | Anthropic Messages | pass | pass | pass | pass |
| Devin | Generative Language | N/A | N/A | N/A | N/A |
| Zed | OpenAI Chat | pass | N/A | pass | pass |
| Zed | OpenAI Responses | pass | pass | pass | pass |
| Zed | Anthropic Messages | pass | pass | pass | pass |
| Zed | Generative Language | pass | pass | pass | pass |
| Codex | OpenAI Chat | N/A | N/A | N/A | N/A |
| Codex | OpenAI Responses | pass | pass | pass | pass |
| Codex | Anthropic Messages | N/A | N/A | N/A | N/A |
| Codex | Generative Language | N/A | N/A | N/A | N/A |
| Copilot | OpenAI Chat | pass | pass | pass | pass |
| Copilot | OpenAI Responses | pass | pass | pass | pass |
| Copilot | Anthropic Messages | pass | pass | pass | pass |
| Copilot | Generative Language | N/A | N/A | N/A | N/A |
| Gemini | OpenAI Chat | N/A | N/A | N/A | N/A |
| Gemini | OpenAI Responses | N/A | N/A | N/A | N/A |
| Gemini | Anthropic Messages | N/A | N/A | N/A | N/A |
| Gemini | Generative Language | pass | pass | pass | pass |
| Antigravity | OpenAI Chat | N/A | N/A | N/A | N/A |
| Antigravity | OpenAI Responses | N/A | N/A | N/A | N/A |
| Antigravity | Anthropic Messages | N/A | N/A | N/A | N/A |
| Antigravity | Generative Language | pass | pass | pass | pass |
| Grok | OpenAI Chat | N/A | N/A | N/A | N/A |
| Grok | OpenAI Responses | pass | pass | pass | pass |
| Grok | Anthropic Messages | N/A | N/A | N/A | N/A |
| Grok | Generative Language | N/A | N/A | N/A | N/A |
| Kiro | OpenAI Chat | N/A | N/A | N/A | N/A |
| Kiro | OpenAI Responses | N/A | N/A | N/A | N/A |
| Kiro | Anthropic Messages | pass | pass | pass | pass |
| Kiro | Generative Language | N/A | N/A | N/A | N/A |
| Factory | OpenAI Chat | pass | pass | pass | pass |
| Factory | OpenAI Responses | pass | pass | pass | pass |
| Factory | Anthropic Messages | pass | pass | pass | pass |
| Factory | Generative Language | pass | pass | pass | pass |
| Cline | OpenAI Chat | pass | pass | pass | pass |
| Cline | OpenAI Responses | N/A | N/A | N/A | N/A |
| Cline | Anthropic Messages | N/A | N/A | N/A | N/A |
| Cline | Generative Language | N/A | N/A | N/A | N/A |
| Qoder | OpenAI Chat | pass | pass | pass | pass |
| Qoder | OpenAI Responses | N/A | N/A | N/A | N/A |
| Qoder | Anthropic Messages | N/A | N/A | N/A | N/A |
| Qoder | Generative Language | N/A | N/A | N/A | N/A |
| WorkBuddy | OpenAI Chat | pass | pass | pass | pass |
| WorkBuddy | OpenAI Responses | N/A | N/A | N/A | N/A |
| WorkBuddy | Anthropic Messages | N/A | N/A | N/A | N/A |
| WorkBuddy | Generative Language | N/A | N/A | N/A | N/A |
| Z Code | OpenAI Chat | N/A | N/A | N/A | N/A |
| Z Code | OpenAI Responses | N/A | N/A | N/A | N/A |
| Z Code | Anthropic Messages | pass | pass | pass | pass |
| Z Code | Generative Language | N/A | N/A | N/A | N/A |
<!-- END GENERATED MATRIX -->
