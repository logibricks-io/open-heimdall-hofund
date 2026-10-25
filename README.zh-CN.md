# hofund

[English](README.md) | **简体中文**

hofund 是 LogiBricks Heimdall 系列中的本地 LLM 网关。它在不同的 LLM API 协议之间做转换，让编码代理指向任意模型，并在多个提供商与账号之间进行故障转移。

Heimdall 是那扇门。Hofund 是守门的剑。

| 仓库 | 角色 |
| --- | --- |
| [open-heimdall](https://github.com/logibricks-io/open-heimdall) | 应用与智能之间的门 |
| [open-heimdall-kernel](https://github.com/logibricks-io/open-heimdall-kernel) | 采用六边形架构的 Rust 网关 |
| open-heimdall-hofund | 本仓库：本地协议网关与代理路由器 |

## 状态

**Pre-alpha。** 规范中间表示（IR），Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 与 Generative Language API 的编解码（含流式），quirk 配置，上游适配器，按顺序的故障转移，本地 HTTP 服务，以及 Claude Code、Codex CLI、Gemini CLI、OpenCode 的配置适配器已经实现。字段对照见 [docs/codecs.md](docs/codecs.md)（英文）。桌面应用还没有实现。

`hofund serve` 默认监听 `127.0.0.1:3425`，除非配置文件另有指定。所有 `/v1` 请求都需要 `hofund token show` 打印的 bearer token。命令有 `serve`、`config check`、`token show`、`token rotate`、`usage`，以及 `agent list`、`agent connect`、`agent disconnect`、`agent status`。

## 计划中的能力

- 以轮辐式（hub-and-spoke）结构在 LLM API 协议之间转换：Anthropic Messages、OpenAI Chat Completions、OpenAI Responses，以及 Generative Language API。每种协议只对照一份规范中间表示（IR）实现一次，而不是做成 N×M 的两两直译。
- 编码代理的配置适配器（Claude Code、Codex、Gemini CLI、OpenCode 等）。适配器先生成修改计划，再把修改应用到代理自己的配置上，使任意代理都能调用任意模型。
- 路由组、选择策略，以及跨提供商和账号的故障转移。
- 长期运行的本地守护进程，以及 `hofund` 命令行工具。
- 菜单栏 / 系统托盘应用。预留路径是 `apps/desktop`（Tauri v2），目前尚未创建。

## 架构

crate 依赖图、单向依赖规则，以及“厂商名称不得出现在 `gateway` 和 `router` 中”的规则，见 [docs/architecture.md](docs/architecture.md)。该文档为英文。

## 构建

安装 [Rust](https://rustup.rs/)。在本仓库中第一次运行 `cargo` 时，会安装 `rust-toolchain.toml` 所固定的编译器（Rust 1.99.1，edition 2024）。

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask help
```

## 参与贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)（英文）。简要规则：除翻译版 `README.<lang>.md` 和未来的 i18n 语言包外，仓库中的一切都使用标准英文。`README.md` 始终是英文，并且是默认文档。

## 致谢

hofund 是受 [magpie](https://github.com/yetone/magpie)（MIT）启发的独立重新实现。它不使用该项目的名称或标志，也不复制其源代码。详见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 许可证

[MIT](LICENSE)。Copyright (c) 2026 LogiBricks。
