# Plugin boundary

Plugins are in-repo Rust. A reviewer reads the diff, and the crate is linked into the `hofundd` process. `cargo xtask check-plugins` is not a sandbox. No static check can stop a determined author who is already allowed to change this repository. The gate exists so an accidental violation, and every obvious deliberate one, fails loudly in CI.

## What is enforced

`check-plugins` scans each package under `plugins/<kind>/` before it runs Cargo, so `build.rs` does not execute until that scan accepts the source. The seam tests call the same scan (`xtask::boundary`).

- Direct dependencies are an allow-list: `hofund-plugin-api` and `hofund-plugin-sdk`. A dev-dependency may also be the matching kit. Tokio features `full`, `net`, `process`, and `signal` are rejected. `hofund-plugin-api` and `hofund-plugin-sdk` must be `workspace = true`. A `path`, `git`, `version`, or registry spec for those crates is rejected, so a same-named crate from another path is not the workspace crate.
- The link closure is Cargo package ids, not names. It is the normal and build dependencies of the workspace `hofund-plugin-api` and `hofund-plugin-sdk`, plus the plugin itself. Dev-dependencies of those crates are not in the allowed set. Target-specific dependencies are included.
- The resolved `hofund-plugin-api` graph must not enable the `kernel` feature. `mod kernel` is private without that feature.
- The source scan rejects `std::process`, `std::net`, `std::os::{unix,windows}::{process,net}`, `tokio::{process,net}`, and the socket and process crates named in the scan. It follows `extern crate` renames and `use` aliases, including a `use` inside a function.
- A `#[path]` that normalizes outside `src/` (the package directory for `build.rs`) is rejected. A `path` attribute nested in `cfg_attr` is rejected outright. `include!`, `include_str!`, and `include_bytes!` must be string literals.
- `cfg(clippy)`, `cfg(not(clippy))`, and `cfg!(clippy)` are rejected. Code hidden from Clippy that way never reaches the lint. `cfg(debug_assertions)`, `cfg(not(debug_assertions))`, and `cfg!(debug_assertions)` are rejected too. A plugin has no reason to branch on the build profile.
- `allow`, `expect`, and `warn` on `clippy::disallowed_*`, `clippy::all`, `clippy::style`, or `clippy::restriction` are rejected, including when they sit inside `cfg_attr`. `clippy::disallowed_types` and `clippy::disallowed_methods` are `clippy::style` lints. `clippy::all` silences them under `-D`.
- Every plugin manifest sets `[lints] workspace = true`. The workspace forbids `unsafe_code`, which is what rejects an FFI block such as `extern "C" { fn system(); }`.
- Clippy then runs with `-F clippy::disallowed_types` and `-F clippy::disallowed_methods`, using `xtask/plugin-boundary/clippy.toml`. `-F` cannot be overridden in source. The config is applied after macro expansion, so `std::$m::Command` is `std::process::Command`. Each package is linted for `--profile dev` and `--profile release`, both with `--all-features`. A package that defines features is also linted with `--no-default-features` on both profiles, so a `cfg(feature)` splice is compiled. CI runs this step on Linux, macOS, and Windows, so `cfg` for those hosts is compiled.

## Known limits

- This is a review aid. A change that passes the gate can still spawn a process if the author writes ordinary Rust that the lints do not name.
- Clippy compiles the host target. The three CI hosts cover Linux, macOS, and Windows. A `cfg` for some other target (for example `wasm32`) is not compiled by this gate.
- A `build.rs` whose only violation is a macro splice the scan cannot see still runs when Cargo builds the package. The scan runs first and rejects literal paths, lint suppression, and `cfg(clippy)`. It does not expand `macro_rules!`.
- A proc-macro already in the API or SDK closure runs at compile time. That is the same trust as the rest of the workspace build.
- String-built command names and calls through function pointers are ordinary Rust. The gate does not try to interpret them.
