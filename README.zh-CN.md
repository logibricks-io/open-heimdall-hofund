# hofund

[English](README.md) | **简体中文**

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-dark.svg">
    <img alt="hofund" src="assets/brand/lockup-light.svg" width="320">
  </picture>
</p>

hofund 是 LogiBricks Heimdall 系列中的本地 LLM 网关。它在不同的 LLM API 协议之间做转换，让编码代理指向任意模型，并在多个提供商与账号之间进行故障转移。

Heimdall 是那扇门。Hofund 是守门的剑。

| 仓库 | 角色 |
| --- | --- |
| [open-heimdall](https://github.com/logibricks-io/open-heimdall) | 应用与智能之间的门 |
| [open-heimdall-kernel](https://github.com/logibricks-io/open-heimdall-kernel) | 采用六边形架构的 Rust 网关 |
| open-heimdall-hofund | 本仓库：本地协议网关与代理路由器 |

## 状态

**0.1.0 已准备。** 规范中间表示（IR），Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 与 Generative Language API 的编解码（含流式），quirk 配置，上游适配器，按顺序的故障转移，本地 HTTP 服务，Claude Code、Codex CLI、Gemini CLI、OpenCode 的配置适配器，以及菜单栏应用已经实现。字段对照见 [docs/codecs.md](docs/codecs.md)（英文）。本仓库还没有推送版本标签，也没有发布 GitHub Release。发布步骤见 [docs/release.md](docs/release.md)（英文）。

`hofund serve` 默认监听 `127.0.0.1:3425`，除非配置文件另有指定。所有 `/v1` 请求都需要 `hofund token show` 打印的 bearer token。命令有 `serve`、`config check`、`token show`、`token rotate`、`usage`，以及 `agent list`、`agent connect`、`agent disconnect`、`agent status`。

## 计划中的能力

- 以轮辐式（hub-and-spoke）结构在 LLM API 协议之间转换：Anthropic Messages、OpenAI Chat Completions、OpenAI Responses，以及 Generative Language API。每种协议只对照一份规范中间表示（IR）实现一次，而不是做成 N×M 的两两直译。
- 编码代理的配置适配器（Claude Code、Codex、Gemini CLI、OpenCode 等）。适配器先生成修改计划，再把修改应用到代理自己的配置上，使任意代理都能调用任意模型。
- 路由组、选择策略，以及跨提供商和账号的故障转移。
- 长期运行的本地守护进程，以及 `hofund` 命令行工具。

## 安装

在本仓库中构建 `hofund` 命令：

```sh
cargo install --path crates/cli --locked
```

这些 crate 的 `publish` 仍为 `false`，因此没有 crates.io 包。

形如 `v0.1.0` 的版本标签会触发发布工作流。该工作流已经写在仓库里，但 0.1.0 还没有因此发布。标签发布之后，资源包括：

- `hofund-cli-installer.sh` 与 `hofund-cli-installer.ps1`，按当前系统下载对应架构的命令行归档
- macOS（arm64、x86_64，以及用 `lipo` 合成的 universal 归档）、Linux（x86_64 与 aarch64）、Windows（x86_64）的归档及 `.sha256` 校验和
- `apps/desktop` 打出的菜单栏安装包，以及单独的 `SHA256SUMS`

签名与公证是可选步骤，缺少仓库密钥时会跳过。详见 [docs/release.md](docs/release.md)（英文）。

菜单栏应用是 `hofund-desktop`。Linux 上需要 WebKitGTK 4.1 与 Ayatana AppIndicator 头文件：

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev patchelf libxdo-dev
cargo build -p hofund-desktop
```

命令行和菜单栏应用在设置了 `HOFUND_CONFIG` 时都读取它。否则使用平台配置目录下的 `hofund/hofund.toml`：Unix 上是 `XDG_CONFIG_HOME` 或 `~/.config`，macOS 上是 `~/Library/Application Support`，Windows 上是 `%APPDATA%`。`--config` 可以改用别的文件。应用在第一次 Start 或 Open config 时写入一份只有默认监听地址的配置。

## 配置

[`examples/hofund.toml`](examples/hofund.toml) 是一份完整示例：两张 `[[providers]]` 表，以及一张 `[[routes]]` 表，其中 `upstreams` 的顺序就是故障转移顺序。`api_key` 可以是字面量，也可以是 `env:NAME`。可以就地检查：

```sh
hofund config check --config examples/hofund.toml
```

`hofund config check` 在文件可以加载时打印 `config ok:` 和它读取的路径。失败时，错误信息以该路径开头。`hofund --help` 和 `hofund <command> --help` 打印对应命令的用法。

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
