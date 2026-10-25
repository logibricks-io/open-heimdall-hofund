# RFC 0001: Microkernel and plugins

- Status: **Proposed**. This document is not the build. The dependency table in [architecture.md](../architecture.md) stays normative until the owner accepts this proposal. No crate moves in this pull request.
- Date: 2026-10-25
- hofund pin: `dfb6e89e64b1eb5d9b489b7f7af2f0685bac41f4` (main after pull request #26)
- Upstream pin: yetone/magpie `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`. `internal/provider/presets.go` is byte-identical at `5b2b3d3f50959735403dff6b77a85eec65772218`. The preset file records that pin at `crates/provider/presets.toml` line 8.
- Workspace version: `0.1.0` (`xtask/tests/workspace_invariants.rs` asserts the manifest). The plugin API below is a design, not a semver promise, until this proposal is accepted.

## Summary

Upstream providers, subscription sign-in, and coding-agent integrations are the three surfaces that several agents need to extend at once. Today they share crates with the gateway spine, so those agents edit the same files.

This proposal keeps the spine as a kernel with a small stable plugin API, a separate helper SDK, and each extension in its own directory. A plugin depends only on the API and, when it needs a helper, the SDK. Plugins do not depend on each other. Kernel source does not name vendors. Stage 1 links plugins at compile time through an explicit registry. A process host for community JavaScript auth plugins is a later stage.

The trait blocks in this document are the proposed contract. This pull request does not compile them.

## 1. Kernel boundary

### What the kernel is today

The spine, from [architecture.md](../architecture.md), is `ir <- codec <- provider <- router <- gateway <- daemon <- cli`. `agent` depends only on `config`. `apps/desktop` sits beside `cli` and depends on `daemon` and `agent`. `xtask/tests/workspace_invariants.rs` function `expected_deps` (line 30) fails the build when a manifest disagrees.

| Kernel piece | Today | Stays because |
| --- | --- | --- |
| IR | `crates/ir`. No I/O. | Every codec and every plugin speaks these types. |
| Codecs | `crates/codec`. `Codec::id` at `crates/codec/src/lib.rs` line 38. Ids in tree: `anthropic-messages`, `openai-chat`, `openai-responses`, `generative-language`, `bedrock-converse` (`crates/codec/src/converse.rs` line 36). | Protocol encode and decode are hub-and-spoke. A vendor adapter calls a codec. It does not own one. |
| Quirks engine | `crates/quirks/src/lib.rs`. `Profile` line 12, `Rule` line 23, `named` line 44, `apply` line 49. Profiles: `responses-default`, `messages-default`, `no-sampling`. | Rules remove or fill canonical fields. Gateway and router do not read profiles (crate header, lines 1-6). |
| Router | `crates/router`. Re-exports `Provider` from `hofund-provider` at `crates/router/src/lib.rs` line 13. Ordered failover. | Routing is product behavior shared by every upstream. |
| Gateway | `crates/gateway/src/lib.rs` lines 1-6. Authenticates, decodes, asks the router, writes the body, records usage (`record` at line 627). | The HTTP edge. Vendor names are forbidden here by `gateway_and_router_do_not_name_vendors` (`xtask/tests/workspace_invariants.rs` line 190), which scans only `crates/gateway` and `crates/router`. |
| Config loader | `crates/config/src/lib.rs`. `Config` line 16, `Provider` line 51, `load` line 281. Raw structs use `deny_unknown_fields` from line 667. Zero workspace dependencies (`expected_deps` returns `&[]`). | Settings stay a leaf. Preset ids stay strings. |
| Secrets | `crates/secrets/src/lib.rs`. `resolve` line 34 (`env:NAME` or a literal). `TokenFile` line 49. | `architecture.md` lines 119-120: the daemon resolves secrets and passes material down. `provider` must not read the store. |
| Usage ledger | `crates/usage/src/lib.rs`. SQLite `requests` table at line 58. `Ledger::record` line 80. | `gateway` records one row per request. `usage` does not depend on `ir`. |
| Daemon | `crates/daemon/src/lib.rs`. `providers` line 231 matches `planned.wire` and constructs `OpenAiCompat`, `Anthropic`, `Generative`, `Vertex::new` (line 261), and `Bedrock::new` (line 275). `plan` line 297 calls `resolve`. | Process wiring. After the seam it accepts a registry and stops naming those types. |
| CLI shell | `crates/cli/src/main.rs` `Command` line 56: `serve`, `config check`, token show and rotate, `usage`, `agent list`, `connect`, `disconnect`, `status`. | A shell. It does not own vendor behavior. |
| Menu-bar shell | `apps/desktop` (`hofund-desktop`). Depends on `daemon` and `agent`. | Same rule as the CLI. |
| TUI shell | Not in the tree. [roadmap.md](../roadmap.md) lines 51-60 describe a future terminal UI as a thin client, with a design doc `docs/tui-design.md` that does not exist yet. | Another shell with the same dependency rule. It is not a plugin. |

### What is coupled today, and therefore leaves the kernel

`Provider` (`crates/provider/src/lib.rs` lines 60-101) has `id`, `count_input` (default `CountOutcome::Unsupported`, line 73), and `execute` (line 96). The same crate also contains every vendor adapter (`anthropic`, `azure`, `bedrock`, `generative`, `mantle`, `openai_compat`, `sigv4`, `vertex`), the HTTP transport (`http.rs`), and the 58-preset catalog (`presets.rs`, `presets.toml`).

`Credential` (`crates/provider/src/http.rs` lines 24-44) includes `GoogApiKey`, documented as the `x-goog-api-key` header the Generative Language API requires. That is a vendor rule living next to the shared transport.

`Wire` (`crates/provider/src/presets.rs` lines 18-32) mixes protocol selection (`Chat`, `Responses`, `Messages`, `Generative`) with vendor URL shapes (`Vertex`, `Converse`). `resolve` (line 103) special-cases `google-vertex` (line 106) and returns an error for `remote-gateway` (line 109).

The daemon match at `crates/daemon/src/lib.rs` lines 251-285 is the composition root, and it names `Vertex` and `Bedrock`. That is why a provider change and a kernel change collide.

`AgentAdapter` (`crates/agent/src/lib.rs` lines 224-316) is the right shape: detect, read, plan, apply, revert, disconnect (drift: unchanged keys restored, user-edited keys kept, `force` restores the backup; the method is lines 271-286), `refresh_token`, `configured_model`, `status`. The four adapters are Rust constants in `crates/agent/src/catalog.rs` (`adapters` line 397). `Slot` (line 363) is `Origin`, `V1`, `Token`, `Literal`, `GeminiModel`, `OpenCodeModels`, `ClaudeModel`, and `ClaudeTier`. `FileKind` (`crates/agent/src/edit.rs` line 9) is `Json`, `Toml`, `Env`. `GatewayTarget` (line 67) is `base_url`, `token`, and `models` (line 77). There is no subscription trait anywhere in the workspace. Vertex ADC token mint (`crates/provider/src/vertex.rs`) is API auth for feature-parity row 2.4, which [subscriptions.md](../coverage/subscriptions.md) line 13 already separates from sign-in.

### Stable plugin API and helper SDK

Two crates. `hofund-plugin-api` is the contract and stays small. `hofund-plugin-sdk` holds helpers that can change without a trait change. A plugin may depend on the API, and on the SDK when it uses a helper. It may depend on nothing else in the workspace.

`hofund-plugin-api` owns:

- the three traits in section 2
- `ProviderError`, `ProviderErrorKind`, `Completion`, and `CountOutcome`, which today sit beside the trait in `crates/provider/src/lib.rs` (lines 43-56) and `crates/provider/src/error.rs`
- protocol ids and a `Wire` enum limited to `Chat`, `Responses`, `Messages`, `Generative`, and `Converse` (the codec's protocol, not a vendor URL builder)
- a credential placement of `Bearer`, `ApiKey`, `XApiKey`, `MessagesBoth`, or `NamedHeader(&'static str)`. The current `Credential::GoogApiKey` arm becomes `NamedHeader("x-goog-api-key")` supplied by the generative plugin. The header bytes on the wire stay the same.
- the account, quota-window, and agent-description types the traits take
- re-exports of the IR request and response types those methods take, so a plugin does not name `hofund-ir`
- `const API: &str = "1"` once this proposal is accepted. A breaking trait change updates every in-tree plugin in the same pull request. There is no out-of-tree plugin in stage 1, and no caret range until a second API version exists. Magpie's lesson is `internal/pluginrt/pluginrt.go` lines 24-27 (`API = "1.0"`, caret ranges on `Manifest.API`).

`hofund-plugin-sdk` depends on `hofund-plugin-api` and owns:

- generic HTTP transports for the five protocols, moved out of `crates/provider/src/http.rs`, `openai_compat.rs` (the non-Mantle path), `anthropic.rs`, and the generative call path. Azure listing and any other plugin call this transport. They do not depend on an OpenAI plugin.
- the SigV4 helper that today is `crates/provider/src/sigv4.rs`. Bedrock is the caller. The kernel does not sign.
- OAuth helpers shared by subscription plugins: PKCE, loopback callback, and device-flow polling. Vendor URLs stay in the plugin.
- retry and paging helpers used by `list_models` and `probe`.

`quirks::apply` is re-exported by the SDK so a plugin can pass a profile id. Adding a `Rule` variant stays a kernel change. The quirks crate continues to carry no vendor name (`crates/quirks/src/lib.rs` lines 41-43). The API does not depend on quirks.

`Request` stays defined in `hofund-ir`. The router depends on `hofund-plugin-api` for the trait and stops depending on `hofund-provider` for it (`crates/router/src/lib.rs` line 13 is the edge that moves). The router does not depend on the SDK. Config keeps preset ids as strings and does not learn the registry. The daemon asks `registry.factory(preset_id)` at startup and returns the existing class of error for an unknown id (`resolve` already errors on an unknown id).

Kernel paths, after adapters have moved, are covered by the vendor-name scan that today covers only gateway and router. The scan stays off those paths while `crates/provider` still contains the adapters. Section 6 says when it turns on.

## 2. Plugin kinds and contracts

Three kinds. Anything else (budget, routing strategies, middleware, the future TUI) stays in the kernel until a later RFC. Magpie's `plugins/` tree at `cbbf316` is product features (`budget`, `gateway`, `middleware`, `otel`, `routing`, `usage`, and the rest). Those are not the extension points that block parallel work here.

A plugin is data when the behavior is a file write or a URL table. It is a crate when the behavior is a credential flow, a signature, a retry, or a write the slot language cannot express.

### 2.1 UpstreamProvider

Covers one implementation, which may register many preset ids. The catalog is 58 `[[preset]]` tables in one file (`crates/provider/presets.toml`, loaded by `include_str!` at `crates/provider/src/presets.rs` line 14). `resolve` special-cases `google-vertex`, `remote-gateway`, and `azure` by id (lines 106, 109, and 128). Every other id selects a URL by wire. The plugin unit is that shared implementation, not one crate per preset id. Data-only presets are TOML fragments. Code plugins in stage 1 are the ones the daemon already special-cases:

| Implementation | Why it is code | Where the behavior lives now |
| --- | --- | --- |
| `openai-compat` | Generic Chat and Responses transport, shared by the URL-only presets. | `crates/provider/src/openai_compat.rs` `OpenAiCompat::new` |
| `anthropic` | Messages transport. | `crates/provider/src/anthropic.rs` |
| `generative` | `generateContent` plus the `x-goog-api-key` placement. | `crates/provider/src/generative.rs`, `Credential::GoogApiKey` |
| `azure` | Deployment listing. The key is `api-key`. | `list_deployments` at `crates/provider/src/azure.rs` line 53 |
| `vertex` | OAuth and URL shape. The body is the generative codec. | `crates/provider/src/vertex.rs`, `models_for` line 195 (static per-location catalog) |
| `bedrock` | SigV4 and the Converse path. The codec stays in the kernel (`bedrock-converse`). | `crates/provider/src/bedrock.rs`, `crates/provider/src/sigv4.rs` |
| `mantle` | Prefix retry from the model id. | `crates/provider/src/openai_compat.rs` lines 3-5 and `crates/provider/src/mantle.rs` |

`count_input` exists and defaults to unsupported. `list_models`, `probe`, and `quota` do not exist on main. Feature-parity rows 2.9 and 2.10 are Missing. Azure listing is the only live list, and it is a function rather than a trait method.

```rust
/// Proposed. Not compiled by this pull request.
#[async_trait]
pub trait UpstreamProvider: Send + Sync {
    /// Static description. The registry keys factories by `spec().id`
    /// and by each preset id in the plugin's TOML fragment.
    fn spec(&self) -> &UpstreamSpec;

    /// Same contract as `Provider::execute` (`crates/provider/src/lib.rs` line 96).
    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError>;

    /// Same default as `Provider::count_input` (line 73).
    async fn count_input(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<CountOutcome, ProviderError> {
        let _ = (request, inbound_protocol);
        Ok(CountOutcome::Unsupported)
    }

    /// Live or catalog model ids for this endpoint.
    /// The default is unsupported, which is every preset on main except the
    /// Azure function `list_deployments`.
    async fn list_models(&self, query: &ListQuery) -> Result<ModelList, ProviderError> {
        let _ = query;
        Err(ProviderError::unsupported("this upstream has no model list"))
    }

    /// One minimal request per speakable protocol. `hofund provider test`
    /// (feature-parity row 2.10, Missing) calls this.
    /// The default builds that request and calls `execute` on the kernel transport.
    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, ProviderError>;

    /// Account balance or key-plan windows, when the vendor publishes them.
    /// The default is unsupported. No balance reader exists in the tree.
    async fn quota(&self, query: &QuotaQuery) -> Result<Vec<QuotaWindow>, ProviderError> {
        let _ = query;
        Err(ProviderError::unsupported("this upstream has no quota endpoint"))
    }
}

pub struct UpstreamSpec {
    pub id: &'static str,
    pub wires: &'static [Wire],
    pub credential: CredentialStyle,
    pub list: ListKind,
}

pub enum ListKind {
    /// Ask the upstream. The plugin owns the URL and the page shape.
    Live,
    /// A static list shipped in the preset fragment. Vertex `models_for`
    /// (`crates/provider/src/vertex.rs` line 195) is this kind.
    Catalog,
    /// `no_list` in `presets.toml` (Vertex sets it at line 67).
    Unsupported,
}

pub enum CredentialStyle {
    Bearer,
    ApiKey,
    XApiKey,
    MessagesBoth,
    NamedHeader(&'static str),
    /// The daemon has already resolved the secret. Vertex and Converse do this
    /// today: `hofund_secrets::resolve` runs in `providers` before `Vertex::new`
    /// and `Bedrock::new` (`crates/daemon/src/lib.rs` lines 236 and 266).
    CallerSupplied,
}
```

A URL-only preset is a fragment, not a crate. The shape already in `presets.toml` (the `anthropic` and `openai` tables at lines 29-47) moves with the implementation that serves it:

```toml
# plugins/providers/openai-compat/presets.toml
[[preset]]
id = "openai"
name = "OpenAI"
chat = "https://api.openai.com/v1"
responses = "https://api.openai.com/v1"
credential = "bearer"
```

The kernel transport speaks `chat` and `responses` from that table. A plugin TOML file names the crate only when one exists:

```toml
# plugins/providers/azure/plugin.toml
id = "azure"
kind = "upstream"
crate = "hofund-plugin-azure"
presets = ["presets.toml"]
```

### 2.2 Subscription

No trait and no sign-in code exist. The contract source is [subscriptions.md](../coverage/subscriptions.md): 33 rows, all missing (lines 9-13). The P1 built-ins (lines 60-61) are Codex (ChatGPT via the Codex backend), the Claude Code bridge, Copilot including GitHub Enterprise Cloud with data residency, and Gemini Code Assist.

Magpie runs those from the provider package. `internal/provider/signin.go` lines 3-9 and 37-42: Codex OAuth at `https://auth.openai.com/oauth/authorize`, loopback `127.0.0.1:1455`, tokens in `logins.json`. Claude is signed in by Claude Code (`claude_signin.go`), which the coverage row states as `claude auth login` rather than a direct OAuth client. Copilot is a device flow. Gemini Code Assist is loopback OAuth. Quota windows are named per row (Codex rate-limit headers, Copilot entitlement, Claude ratelimit headers, Gemini `retrieveUserQuota`).

Stage 1 implements those four as native Rust plugins, one directory each. Copilot on a GitHub Enterprise host is a host field of the Copilot plugin (the coverage table calls it a host variant), not a second crate. The 13 community plugins that magpie no longer serves from core (coverage lines 16 and 31-42) wait for the process host in section 3.

```rust
/// Proposed. Not compiled by this pull request.
#[async_trait]
pub trait Subscription: Send + Sync {
    fn id(&self) -> &str;

    /// PKCE, device code, a local token file, or a bridge that drives
    /// another tool's own sign-in. Declarative where the coverage row
    /// is a URL and a callback. The HTTP steps stay in the plugin.
    fn flow(&self) -> SignInFlow;

    async fn start_sign_in(&self, ctx: &SignInCtx) -> Result<SignInChallenge, PluginError>;

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
    ) -> Result<Account, PluginError>;

    async fn refresh(&self, account: &Account) -> Result<Account, PluginError>;

    /// Windows such as five-hour and seven-day. Empty when the vendor
    /// publishes none. The shared `QuotaWindow` type is the coverage row
    /// "subscription usage core".
    async fn quota(&self, account: &Account) -> Result<Vec<QuotaWindow>, PluginError>;

    /// Models this account can see. Distinct from `UpstreamProvider::list_models`,
    /// which lists a key-backed endpoint.
    async fn list_models(&self, account: &Account) -> Result<Vec<String>, PluginError>;

    fn accounts(&self, pool: &AccountPool) -> Vec<AccountId>;

    /// The hop the router calls for this signed-in account.
    ///
    /// Codex returns the ChatGPT backend client. Copilot returns the Copilot
    /// API client. Gemini Code Assist returns the Code Assist client. Claude
    /// returns the bridge client. The value is an ordinary `UpstreamProvider`.
    /// The router never sees `Subscription`.
    fn upstream(&self, account: &Account) -> Arc<dyn UpstreamProvider>;
}

pub enum SignInFlow {
    /// Codex: authorize URL, PKCE, loopback callback.
    Pkce { authorize_url: &'static str, callback: Callback },
    /// Copilot, including a GitHub Enterprise host.
    Device { code_url: &'static str },
    /// Read a token file the vendor's own tool already wrote.
    /// Codex's `~/.codex/auth.json` (`CODEX_HOME`) is the coverage example.
    TokenFile { path_env: &'static str, relative: &'static str },
    /// Claude: `claude auth login` in a hofund-owned config dir.
    /// The plugin does not speak Anthropic's OAuth token endpoint.
    Bridge { command: &'static str },
}
```

`AccountPool` lives in the API. Plugins do not share a process-global login file. The daemon passes the pool in, the same way it passes resolved secrets into providers today.

#### How an account becomes a router member

Feature-parity row 2.12 models keys as router members. Row 3.7 does the same for accounts. The subscription plugin does not insert itself into the router.

1. Config names a subscription id and which accounts to serve (one id, or every account in the pool). Preset ids stay strings.
2. At startup the daemon loads the plugin, calls `accounts`, then `upstream` once per selected account.
3. Each returned `Arc<dyn UpstreamProvider>` is one router member. The member id is `{subscription}/{account}`. Failover, order, and cooldown are the router's existing behavior. A plugin pull request does not edit `crates/router`.

Row 3.8 is the shared quota model: windows, resets, and credits, one shape for routing, the CLI, and the UI. `Subscription::quota` fills that shape. The daemon reads it. An exhausted window is a cooldown on that member, which is how failover already skips a member. The plugin returns windows. It does not call the router, and it does not import `hofund-router`.

### 2.3 AgentIntegration

`Described` in `crates/agent/src/catalog.rs` already implements `AgentAdapter` from a const `Description`. Connect, disconnect, and drift are engine behavior (`disconnect` at `crates/agent/src/lib.rs` lines 263-278). A plugin that only names paths and keys supplies data. The engine performs every write, takes the `*.hofund.bak` backup, and restores user-edited keys.

`FileKind` has no YAML. Goose and Hermes need YAML ([agents.md](../coverage/agents.md) describes their files). That is one kernel change, `FileKind::Yaml`, not a crate per agent. Per-OS paths are data: `Session` already reads `HOME` or `USERPROFILE`, `PATH`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GEMINI_CLI_HOME`, and `XDG_CONFIG_HOME` (`from_process_env`, lines 108-111).

On this pin the four consts already write more than a URL and a token:

- Gemini CLI writes `model.name` (`catalog.rs` lines 176-178 and 246, `Slot::GeminiModel` at line 369). A string `model` is moved there. [agents.md](../coverage/agents.md) line 18 records the same fact.
- OpenCode writes one `models` entry per route, with `name` and `variants` set to `{}` (lines 267-270, `Slot::OpenCodeModels` at line 370). Modalities, limits, and reasoning are omitted because this build has no catalog values.
- Claude Code writes `env.ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, and `ANTHROPIC_DEFAULT_FABLE_MODEL` only for a Fable id (lines 31-37 and 72-94, `Slot::ClaudeTier` at line 375). `CLAUDE_CODE_SUBAGENT_MODEL`, `effortLevel`, and `CLAUDE_CODE_EFFORT_LEVEL` stay unset.

A crate exists only when a slot cannot express the write. Known cases from the coverage inventory:

- an OS credential store (the Zed row)
- an OS environment store (cursor-local writes `launchctl`, `HKCU`, or `environment.d`; there is no file)
- a sealed file (Hanako's AES-GCM catalog)

Claude tiers are data: `Slot::ClaudeTier` already exists. OpenCode's models object is one engine slot, not a crate. Filling name, modalities, limits, and reasoning is that same slot once `GatewayTarget` carries a model spec. The spec comes from the kernel catalog (feature-parity rows 2.8 and 2.14). A plugin does not fetch models.dev.

#### Data, kernel commands, and code

| Need | Row | Kind | Where it lives |
| --- | --- | --- | --- |
| Selected model key | 5.5, `magpie claude opus` | Data | `model` on `AgentDescription`. The CLI command `hofund agent set <agent> model=<id>` is a kernel shell. It checks the id against the catalog and then calls the engine. |
| Effort key | 5.5 | Data | `effort` on `AgentDescription`, written by the same command. Claude's effort keys stay optional data. They are not a Claude crate. |
| Visible model list | 5.7 | Data | `visible` on `AgentDescription`: all routes, or an allow-list. The engine skips other ids when it writes a models object. |
| Profiles | 5.6 | Kernel | A profile is a TOML snapshot of every agent's files. It is not a plugin hook. One kernel command saves and restores the set. |
| Model catalog in the agent file | 2.8, 2.14 | Data, one engine slot | `Slot::CatalogModels`. OpenCode is the first caller: `name`, modalities, context, output limit, and reasoning variants, taken from the kernel spec. Empty `variants` remains the fallback when the spec has no reasoning levels, which is what `catalog.rs` lines 268-270 do today. |
| Picker-only agents | [agents.md](../coverage/agents.md) line 102 | Data | `cursor`, `copilot`, and `devin` set `route = "none"` and write only the model id. No base URL. |
| Special gateway routes | [agents.md](../coverage/agents.md) lines 104-109 | Kernel routes, named by data | The description may set `requires` to `ollama`, `muse-models`, or `model-capabilities`. The gateway implements them: Ollama `/api/version`, `/api/tags`, `/api/show`, `/api/chat`; `/muse-code/models`; `/v1/models` with `api_types` and `capabilities`. An agent plugin does not register an HTTP route. Codex `/backend-api/codex` is the same kind of kernel route (line 109). |

```rust
/// Proposed. Not compiled by this pull request.
pub trait AgentIntegration: Sync {
    fn description(&self) -> &AgentDescription;

    /// Default: the kernel engine. Same steps as `Described`
    /// (`crates/agent/src/catalog.rs`).
    fn plan(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, PluginError> {
        engine::plan(self.description(), session, target)
    }
    fn apply(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, PluginError> {
        engine::apply(self.description(), session, target)
    }
    fn disconnect(
        &self,
        session: &Session,
        force: bool,
    ) -> Result<DisconnectOutcome, PluginError> {
        engine::disconnect(self.description(), session, force)
    }
    fn status(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<ConnectionStatus, PluginError> {
        engine::status(self.description(), session, target)
    }
}

pub struct KeyPath {
    pub file: &'static str,
    pub key: &'static str,
}

pub struct AgentDescription {
    pub id: &'static str,
    pub display_name: &'static str,
    pub binary: &'static str,
    pub files: &'static [AgentFile],
    pub sets: &'static [OwnedSet],
    pub model: Option<KeyPath>,
    pub effort: Option<KeyPath>,
    /// Row 5.7. `All` writes every route. `Only` is the allow-list.
    pub visible: VisibleModels,
    /// `Gateway` writes the base URL. `None` is picker-only (cursor, copilot, devin).
    pub route: AgentRoute,
    /// Kernel routes this agent needs. The plugin does not serve them.
    pub requires: &'static [&'static str],
}

pub struct AgentFile {
    pub id: &'static str,
    pub kind: FileKind,
    pub paths: OsPaths,
}

pub struct OsPaths {
    pub linux: &'static str,
    pub macos: &'static str,
    pub windows: &'static str,
    /// Env var that replaces the directory, as `CODEX_HOME` does today.
    pub env_override: Option<&'static str>,
}

pub struct OwnedSet {
    pub file: &'static str,
    pub key: &'static str,
    pub slot: Slot,
}

pub enum FileKind { Json, Toml, Env, Yaml }

pub enum Slot {
    Origin,
    V1,
    Token,
    Literal(&'static str),
    /// Claude tier env. `fable` is skipped unless the model id is Fable.
    /// Same rule as `Slot::ClaudeTier` at `catalog.rs` line 375.
    ClaudeTier(&'static str),
    /// One models object per visible route, filled from the kernel model spec.
    CatalogModels,
}

pub enum VisibleModels { All, Only(&'static [&'static str]) }

pub enum AgentRoute { Gateway, None }
```

The Codex const (`catalog.rs` from line 64) becomes:

```toml
# plugins/agents/codex/agent.toml
id = "codex"
display_name = "Codex CLI"
binary = "codex"

[[file]]
id = "config"
kind = "toml"
linux = "~/.codex/config.toml"
macos = "~/.codex/config.toml"
windows = "~/.codex/config.toml"
env_override = "CODEX_HOME"

[[set]]
file = "config"
key = "model_provider"
slot = { literal = "hofund" }

[[set]]
file = "config"
key = "model_providers.hofund.base_url"
slot = "v1"

[[set]]
file = "config"
key = "model_providers.hofund.experimental_bearer_token"
slot = "token"

[model]
file = "config"
key = "model"
```

The first migration of this agent must produce a byte-identical plan against the Rust const. New agents are TOML only.

## 3. Registration and loading

### Compile time

Each plugin that has code is one crate. A feature on the registry crate selects it. Two mechanisms can collect the crates:

| Mechanism | What it gives | What it costs on this repo |
| --- | --- | --- |
| Explicit registry | `plugins/registry/build.rs` reads every `plugin.toml`, emits a `match` from id to factory, and the registry `Cargo.toml` lists one feature and one dependency per implementation. `xtask` fails the build when the list is unsorted, missing, or has a duplicate id. | One shared manifest. Two pull requests that append different lines conflict in one hunk. Resolution is to keep both lines and re-run the check. |
| `linkme` or `inventory` | Each crate registers itself through a distributed slice. No central list. | The workspace already treats an explicit member list as the review surface (`MEMBERS` at `xtask/tests/workspace_invariants.rs` line 11). A missing registration on one target is a silent omission. CI runs the test job on Windows (`desktop` and `test` matrix). Distributed slices depend on the platform linker. `cargo test -p <plugin>` passes when the plugin's tests pass even if the binary never linked it. |

### Runtime

Magpie's community OpenCode auth plugins are JavaScript, run on Bun. The coverage file says so (line 16) and points at `github.com/magpie-community/plugins`, which that inventory did not review. The host in tree at the pin is `internal/plugin/host.go`: it embeds `host.js` (line 33) and `pi.js` (line 40), speaks JSON lines (`message` at line 43), and imports `os/exec` (line 18). `Loaded.Provides` (line 499) are the provider ids an auth hook signs in to. `Clash` (lines 509-517) records two plugins that claim one id. The host runs one of them.

A WASM host does not load those JavaScript modules. This workspace forbids `unsafe` (`AGENTS.md`). Streaming HTTP and OS credential stores need a capability host either way. Stage 1 does not build that host.

### Stage 1 recommendation

Compile-time explicit registry, generated from `plugin.toml`.

The set of plugins is visible in review, which matches how `MEMBERS` and `expected_deps` already work. A duplicate preset id fails in `xtask` on every OS, including the Windows test job. `cargo test -p hofund-plugin-azure` proves that plugin. The registry crate is the only package whose manifest grows, and it grows by sorted append-only lines.

Feature flags live on `hofund-plugin-registry`, one per implementation crate, default features naming all of them so `hofund` links every in-tree plugin. A developer builds a subset with `--no-default-features --features azure`. Data-only presets and data-only agents have no feature, because they have no crate. The registry's generated code calls the kernel transport for those ids.

`plugins/registry` is the only crate allowed to depend on plugin crates. `hofund-daemon` takes the registry as an argument and does not depend on plugin crates, so a plugin pull request does not edit the daemon manifest. `hofund-cli` and `hofund-desktop` depend on the registry and become the composition roots. Today the daemon is that root (`architecture.md` crate table, and `providers` at `crates/daemon/src/lib.rs` line 231). The migration pull request that introduces the registry updates the architecture table in the same change.

The process host for the 13 community subscriptions is a later RFC. It would be a program outside the kernel speaking a line protocol, following `internal/plugin/host.go`, and it would register through the same `Subscription` trait. Coverage priority P3 already says to choose a host or a native port before implementing that set.

## 4. Target layout, names, and dependency rules

### Layout

End state, after the mechanical moves in section 6. The first pull requests add `crates/plugin-api` and `crates/plugin-sdk` beside the current crates so the tree does not move in one change.

```text
crates/kernel/ir
crates/kernel/codec
crates/kernel/quirks
crates/kernel/router
crates/kernel/gateway
crates/kernel/config
crates/kernel/secrets
crates/kernel/usage
crates/kernel/daemon
crates/kernel/cli
crates/kernel/plugin-api
crates/kernel/plugin-sdk
apps/desktop
plugins/providers/<id>          # plugin.toml, presets.toml, src/ when the plugin has code
plugins/subscriptions/<id>
plugins/agents/<id>             # agent.toml, src/ only when a slot is not enough
plugins/registry                # build.rs + the append-only manifest
kits/upstream
kits/subscription
kits/agent
xtask
```

Until that move, paths stay `crates/<name>` and the new crates are `crates/plugin-api` and `crates/plugin-sdk`. Plugins may land under `plugins/` before the kernel directory exists. `xtask` keys off the package name and the path prefix, not off the kernel directory.

### Names

- Plugin id: kebab-case ASCII, equal to the coverage inventory id when one exists (`codex`, `claude-code`, `google-vertex`, `remote-gateway`).
- Crate name: `hofund-plugin-<id>`. Package path: `plugins/<kind>/<id>`.
- One implementation crate may own many preset ids. The fragment lists them. The id of the crate is the implementation id (`openai-compat`), and each `[[preset]]` id is the catalog id (`openai`, `deepseek`, and the other URL-only rows that share it).
- `remote-gateway` stays a preset id that `resolve` rejects (line 109). It does not become a kernel type.
- Protocol ids stay the codec ids. Vendor names do not appear in kernel paths or, after the scan widens, in kernel source.

### Rules `xtask` enforces when the first plugin lands

This pull request does not add these checks. The pull request that adds `plugins/` adds them next to `dependency_graph_matches_architecture`.

1. A package whose path starts with `plugins/` may name `hofund-plugin-api` and, when it uses a helper, `hofund-plugin-sdk`. No other workspace dependency. Third-party crates (`reqwest`, `ring`, and the crates a signature needs) are allowed. `hofund-gateway`, `hofund-daemon`, `hofund-router`, `hofund-provider`, `hofund-ir`, and any other `hofund-plugin-*` are rejected.
2. No plugin depends on another plugin. This is the rule `TestPluginsImportOnlyCore` enforces for Go imports (`internal/pluginrt/pluginrt_test.go` line 94, described at `pluginrt.go` lines 8-10). Here it is a Cargo dependency check plus a walk of `use` paths.
3. `plugins/registry` is the only package that may depend on `hofund-plugin-*` crates. Its dependency list and its feature list are sorted and equal to the `plugin.toml` files on disk.
4. Preset ids are unique across fragments. The 58-id assertion that the catalog tests encode today moves to this check. Aliases stay aliases (the four `[[alias]]` tables at `presets.toml` lines 10-27).
5. The vendor-name scan grows from `crates/gateway` and `crates/router` to every kernel crate after the adapters have left `crates/provider`. Turning it on earlier fails the build on the adapters that are still there on purpose.
6. A plugin's dev-dependencies may include the kit for its own kind (`kits/upstream`, `kits/subscription`, or `kits/agent`) and ordinary test crates. A provider plugin does not depend on the agent kit.
7. `hofund-config` keeps zero workspace dependencies. Preset ids stay strings. Unknown ids fail at daemon startup, as `resolve` fails today.
8. No runtime crate depends on `xtask`.

`docs/architecture.md`, the crate manifest, and `expected_deps` still change together for every new edge. The edges this proposal adds, once accepted, are: `hofund-plugin-sdk` depends on `hofund-plugin-api`; `hofund-provider` (while it still exists) and `hofund-router` depend on `hofund-plugin-api`; `hofund-cli` and `hofund-desktop` depend on `hofund-plugin-registry`; plugin packages depend on `hofund-plugin-api` and may depend on `hofund-plugin-sdk`.

## 5. Conformance kits and real-tool acceptance

Each plugin pull request is self-verifying. The kit is shared so the plugin author does not copy the harness. The kit depends on the plugin API, the SDK, and test tools. It does not depend on a plugin.

| Kind | Kit | Proves | Grown from |
| --- | --- | --- | --- |
| Upstream | `kits/upstream` | `execute` against wiremock, auth header placement, error class (`Connect`, `Timeout`, `Status`, `Decode`), `list_models` pagination when `ListKind::Live`, `probe` sends one minimal request and records a status, `quota` either returns windows or the unsupported error. | `crates/provider/tests/upstream.rs` (header: "Upstream adapters against a local mock. No real network.") and `crates/provider/tests/azure_deployments.rs`. |
| Subscription | `kits/subscription` | Recorded HTTP fixtures for `start_sign_in`, `complete_sign_in`, `refresh`, and `quota`. No browser, no loopback to a vendor, no live OAuth in CI. | No hofund test exists yet. The fixture format is request and response files checked into the plugin directory. |
| Agent | `kits/agent` | Temp `Session` (the pattern in `crates/agent/tests/rewrite.rs`), golden bytes before and after `plan`, `apply` backup, `disconnect` drift (user-edited keys kept, untouched keys restored), `refresh_token` without a new backup. | `crates/agent/tests/rewrite.rs`. |

A plugin pull request runs `cargo test -p <crate>` for a code plugin, or the registry's fragment test for a data-only plugin, plus `cargo test -p xtask --test workspace_invariants`. The fragment test loads every `presets.toml` and `agent.toml` and checks ids, required keys, and duplicate detection.

Golden files live next to the plugin: `plugins/agents/codex/fixtures/`. Subscription recordings live at `plugins/subscriptions/codex/fixtures/`.

### How this meets the evidence rule

[feature-parity.md](../feature-parity.md) status key: a row is Done only with the code location, the tests, and the CI run that executed them. Behavior that a real agent or a real upstream has not exercised says `verified with mocks only` or `unverified`. A plugin pull request that lands mocked tests marks the coverage cell Partial and writes `verified with mocks only` plus the CI run URL. It does not mark the row Done.

### Real-tool acceptance

Required CI has no vendor secrets and does not log in to a browser. Acceptance is optional and sits outside the plugin:

- `kits/accept/<id>.md` names the command (`hofund provider test <id>`, or the subscription sign-in command once it exists) and the observation that counts as success.
- A `workflow_dispatch` job may run one id when an operator supplies secrets. It is not a required check.
- The feature-parity row cites that run's URL when it has one. Otherwise the row says `unverified`.

`crates/daemon/tests/http.rs` stays the kernel test that the gateway still serves a routed request after the registry is wired. Plugin authors do not edit it unless the route shape changes.

## 6. Migration and parallel work

No step below starts until the owner accepts this proposal. This pull request does not add a plugin, a `CODEOWNERS` file, or a crate.

In flight, and not part of the proposal:

- Pull request #26 was squash-merged as `dfb6e89`. This pin already writes Gemini `model.name`, OpenCode `models` entries, and the Claude tier env vars (section 2.3).
- Pull request #28 (`cursor/provider-test-lists-692a`) is open and unmerged. It sketches `list_models`, `diff_ids`, `refresh_due`, and `run_probe`, and wires them through the daemon and the CLI ahead of this API. It stays unmerged until `UpstreamProvider::list_models` and `UpstreamProvider::probe` exist. After pull request B it rebases onto those methods. It is not the trait, and it is not a plugin.

### Pull requests, in order

Each one is branched from latest `main`. Each one updates `docs/architecture.md` and `expected_deps` when it adds an edge.

| Id | Change | Parallel with |
| --- | --- | --- |
| B | Add `hofund-plugin-api` (traits, types, errors, `API`) and `hofund-plugin-sdk` (empty of transports until D, so the dependency edge exists). `hofund-provider` and `hofund-agent` re-export the traits. No behavior change. Existing tests stay where they are. | Nothing. This is the seam. |
| C | `hofund-router` depends on `hofund-plugin-api` for the trait. The daemon match on `Wire` stays. | Nothing. |
| D | `plugins/registry`, and move the generic transports, SigV4, retry, and paging helpers into `hofund-plugin-sdk`. The daemon's Chat, Responses, Messages, and Generative arms call `registry.factory`. `Vertex` and `Bedrock` stay in the match until their plugins exist. `xtask` plugin rules turn on for the new path. | Nothing. |
| E | One implementation per pull request: azure, vertex, bedrock, mantle, then the URL-only fragments that ride `openai-compat`, `anthropic`, and `generative`. Each pull request moves that code and its `[[preset]]` rows. `hofund-provider` keeps a re-export until the last caller is gone, so `crates/provider/tests/upstream.rs` keeps compiling during the move. The plugin itself depends only on the API. | The E pull requests touch disjoint directories. They share only the sorted registry manifest. |
| F | Agent loader: if `plugins/agents/<id>/agent.toml` exists, build the description from it, otherwise use the Rust const. First file is `codex`, and its plan is byte-identical to the const. Then `claude-code`, `gemini-cli`, and `opencode` the same way, including `model.name`, `ClaudeTier`, and the OpenCode `models` object as they behave on this pin. | F starts once B has the `AgentDescription` type. It does not wait for E. `agent` does not depend on `provider` today. |
| G | `FileKind::Yaml` in the kernel engine, with a kit test. Then the remaining P1 agents as TOML: pi, goose, zed, vscode, crush, hermes, kimi, qwen, droid, cline, grok. A crate is added only for a write the slots cannot express, in its own pull request. | G's TOML files are disjoint. The YAML change is one kernel pull request before goose and hermes. |
| H | Subscription plugins, one directory each: `codex`, `claude`, `copilot` (host field covers the Enterprise variant), `gemini`. Each implements `upstream` so a signed-in account is a router member (rows 2.12 and 3.7). Quota windows feed member cooldown (row 3.8). | H starts once D has the SDK transports. The four directories do not overlap. Codex joined mode also waits on the kernel `/backend-api/codex` route. |
| I | Delete `crates/provider/presets.toml` once every fragment loads through the registry. The catalog test concatenates fragments and still asserts the 58 ids and the four aliases. Widen the vendor-name scan to kernel crates in this pull request, when `crates/provider` no longer contains adapters. | After E. |
| J | Move `crates/<name>` to `crates/kernel/<name>`. Mechanical. No behavior change. | After I, or earlier if the owner wants the paths sooner. It touches every import and should not run beside feature work. |

Quirks stay in the kernel for the whole sequence. A new `Rule` variant is a kernel pull request. Vendor behavior stays in the plugin that calls `quirks::apply` through the SDK.

### Parallel work plan

B, C, and D are serial. After D, three batches run at the same time. Counts are pull requests, one directory each unless the row says otherwise.

| Batch | Starts after | Pull requests | Concurrent agents | Merge constraint |
| --- | --- | --- | --- | --- |
| E providers | D | About 8. Four code plugins (azure, vertex, bedrock, mantle) and about four fragment batches (the URL-only presets on `openai-compat`, `anthropic`, and `generative`, plus the six ids `resolve` still rejects). | Up to 4, one directory each. | Each appends a sorted block to `plugins/registry`. Two merges conflict only in that hunk. Keep both lines and re-run `xtask`. I waits until every E pull request has merged. |
| F and G agents | D for the loader. The loader is F's first pull request and is serial. | F is 5 (loader, then codex, claude-code, gemini-cli, opencode). G is about 12 (one `FileKind::Yaml` kernel change, then the P1 agents pi, goose, zed, vscode, crush, hermes, kimi, qwen, droid, cline, grok). | After the loader, the four F migrations and the non-YAML G files run together. Goose and hermes wait on the YAML pull request. Zed is a code plugin when the slot cannot write the OS credential store. | Agent directories are disjoint. The YAML change is the one shared kernel edit in this batch. `CatalogModels` is part of the loader, not a per-agent pull request. |
| H subscriptions | D | 4: codex, claude, copilot, gemini. | 4. | Same registry-append rule as E. A plugin that needs a kernel route waits for that route: Codex `/backend-api/codex`, Muse `/muse-code/models`, Ollama emulation. Those routes are kernel pull requests, not rows in this batch. |

Pull request #28 does not join a batch. It rebases onto B once `list_models` and `probe` are on the trait, and it merges only after that.

Assign one agent per directory. Do not assign two agents to the registry crate. The kernel owner merges B, C, D, the YAML change, the special gateway routes, I, and J.

The six presets that `resolve` still rejects (`typesafe`, `vercel-jev`, `cloudflare-jev`, `bailian-decision`, `qwen-decision`, `remote-gateway`, from [providers.md](../coverage/providers.md)) are fragment or code pull requests after D. They are not a reason to keep `presets.toml` inside the kernel.

### Who owns which path

One agent per batch. A batch is a set of directories that do not overlap.

The only shared file in a batch is `plugins/registry/Cargo.toml` (features and dependencies, sorted, append-only) and the generated registry, which is rebuilt rather than hand-edited. Two agents appending different lines produce a one-hunk conflict. The resolution is to keep both lines, sort them, and re-run `xtask`. Nobody hand-edits a `match` inside `crates/daemon` or `crates/kernel`.

`CODEOWNERS` is added by the first migration pull request, not by this one. This document does not name GitHub users or teams. The owner fills the handles. The path globs are:

```text
/crates/plugin-api/
/crates/plugin-sdk/
/crates/kernel/
/crates/daemon/
/crates/router/
/crates/gateway/
/plugins/registry/
/plugins/providers/
/plugins/subscriptions/
/plugins/agents/
/kits/
```

Kernel globs and `plugins/registry` belong to whoever the owner assigns to the kernel. Each plugin batch's pull request adds the lines for the directories it introduces, naming the people the owner assigns to that batch. A plugin agent does not need write access to another plugin's directory.

`docs/magpie-comparison.md` gains the adopt-or-redesign rows for this seam in pull request B, after acceptance. This proposal does not edit that table, so an unaccepted design is not recorded as a decision.

## 7. Comparison with magpie at `cbbf316`

Pins: hofund `dfb6e89e64b1eb5d9b489b7f7af2f0685bac41f4`, magpie `cbbf3163cb3c87ee0c327a56bc5689ddc5eda947`.

### What we adopt

| Magpie | Where | Hofund |
| --- | --- | --- |
| The runtime never imports a plugin, and a plugin never imports another. They meet through services. | `internal/pluginrt/pluginrt.go` lines 8-10. Test: `internal/pluginrt/pluginrt_test.go` line 94. `main.go` and `plugins/all` are the only gatherers (test comment, line 91). | The registry crate is the only gatherer. `xtask` rejects a plugin-to-plugin dependency. The daemon does not import plugins. |
| One plugin per provider id, and a recorded clash when two claim it. The host runs the user's choice, otherwise the last one listed. | `internal/plugin/host.go` `Loaded.Provides` line 499, `Clash` lines 509-517, `Clashes` line 521. | The registry rejects a duplicate preset id at build time. Stage 1 has no user-installed overlay, so a duplicate is a hard error rather than a runtime preference. |
| A plugin agent declares the file, the format, the model key, and the prefix. The host performs every write and stashes the user's previous values. | `internal/agentplug/agentplug.go` `Desc` lines 65-83, `Formats` lines 57-63. `internal/agent/plugged.go` lines 3-10 and `pluggedStash` line 48. A plugin id that collides with a built-in is the built-in (`plugged.go` lines 31-34). | `agent.toml` plus the existing engine (`Described`, `disconnect` drift). A data plugin does not write files itself. A colliding id fails the registry check. |
| An API version constant on the manifest. | `internal/pluginrt/pluginrt.go` line 27, `const API = "1.0"`. | `hofund_plugin_api::API = "1"` after acceptance. Helpers live in `hofund-plugin-sdk` and can change without a new `API` value. Caret ranges wait until a second version exists. |
| Presets are data. | `internal/provider/presets.go`, recorded as `crates/provider/presets.toml` (58 presets, `source_commit` at line 8). | Preset fragments stay TOML. The implementation crate is shared. |
| Built-in product plugins fail independently. | `pluginrt.go` lines 1-6: one that fails or panics is `FAILED` and the rest run. User toggles live in `plugins.yml`. | Out of scope for the three extension kinds. Noted so a later RFC can reuse it for optional product features. Stage 1 links every in-tree provider, subscription, and agent. |
| Probe and list live next to the provider. | `internal/provider/test.go` `Provider.Test` line 37, `AuthHeaders` line 457, `probe` line 500. `internal/provider/autolist.go` `MinRefreshEvery` line 39 (10 minutes), `listRetry` line 231 (15 minutes), `KeepListsFresh` line 239. | The methods are `UpstreamProvider::probe` and `list_models`. The numbers and the URL rules are specified in the plugin pull request that implements them, not in this proposal. Pull request #28 is open and stays unmerged until those methods exist. |

### What we redesign

| Magpie | Where | Hofund |
| --- | --- | --- |
| Product plugins under `plugins/` (budget, routing, otel, middleware, sessions, tools, usage, and the others listed by `git ls-tree` of `plugins` at the pin). | `plugins/*`, gathered by `plugins/all`. | Those stay kernel features until their own RFC. The plugin kinds in this document are only upstreams, subscriptions, and coding-agent integrations. |
| Community auth plugins run in-process JavaScript (moejs for agent modules, Bun for OpenCode auth plugins). | `internal/agentplug/agentplug.go` lines 18-19 (`moejs`, no Node). Coverage line 16 for Bun. `internal/plugin/host.go` is the process-and-line-protocol host. | Stage 1 is native Rust, compiled in. The 13 community subscriptions are a later process host. WASM is the wrong vehicle for those JavaScript plugins. |
| Sign-in, presets, probes, and every vendor client share `internal/provider`. | `internal/provider/signin.go`, `presets.go`, `test.go`, `autolist.go`, plus the per-vendor files named in the coverage tables. | Generic transports, SigV4, and OAuth helpers live in the plugin SDK. The API stays the traits. Vendor URL tables are fragments. Vendor credential code is a plugin crate. The kernel match in `crates/daemon/src/lib.rs` lines 251-285 goes away as those plugins land. `Subscription::upstream` is how a signed-in account becomes one of those clients. |
| Agent plugins are JavaScript modules named by `package.json` `magpie.agent`, with a 1 second load limit and a 1 second connect limit. | `internal/agentplug/agentplug.go` lines 1-4, 119-120. | Declarative TOML loaded by the Rust engine. Limits are irrelevant because there is no embedded interpreter in stage 1. |
| `plugins.yml` can turn a built-in off. | `pluginrt.go` lines 3-6 and 84. | Stage 1 links the in-tree set. A feature flag on the registry crate is how a developer builds a subset. Users do not disable a provider plugin in a config file in stage 1. |

## Sign-off

Acceptance means the owner agrees to the kernel boundary, the three traits, the compile-time registry, and the migration order. It does not merge any of the migration pull requests by itself.

Until then:

- `docs/architecture.md` is the build.
- Coverage cells and feature-parity statuses stay as they are.
- Pull request #28 stays unmerged until the trait it implements exists.
