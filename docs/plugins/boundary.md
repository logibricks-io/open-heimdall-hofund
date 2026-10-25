# Plugin boundary

Plugins are in-repo Rust. A reviewer reads the diff, and the crate is linked into the `hofundd` process. `cargo xtask check-plugins` is not a sandbox. No static check can stop a determined author who is already allowed to change this repository. The gate exists so an accidental violation, and every obvious deliberate one, fails loudly in CI.

## What is enforced

`check-plugins` scans each package under `plugins/<kind>/` before it runs Cargo. A plugin `build.rs` is rejected in that scan, so Cargo never runs it. The seam tests call the same scan (`xtask::boundary`).

- Direct dependencies are an allow-list: `hofund-plugin-api` and `hofund-plugin-sdk`. A dev-dependency may also be the matching kit. Tokio features `full`, `net`, `process`, and `signal` are rejected. `hofund-plugin-api` and `hofund-plugin-sdk` must be `workspace = true`. A `path`, `git`, `version`, or registry spec for those crates is rejected, so a same-named crate from another path is not the workspace crate.
- The link closure is Cargo package ids, not names. It is the normal and build dependencies of the workspace `hofund-plugin-api` and `hofund-plugin-sdk`, plus the plugin itself. Dev-dependencies of those crates are not in the allowed set. Target-specific dependencies are included.
- The resolved `hofund-plugin-api` graph must not enable the `kernel` feature. `mod kernel` is private without that feature.
- The source scan rejects `std::process`, `std::net`, `std::os::{unix,windows}::{process,net}`, `tokio::{process,net}`, and the socket and process crates named in the scan. It follows `extern crate` renames and `use` aliases, including a `use` inside a function.
- A `#[path]` that normalizes outside `src/` (the package directory for `build.rs`) is rejected. A `path` attribute nested in `cfg_attr` is rejected outright. `include!`, `include_str!`, and `include_bytes!` must be string literals.
- `cfg`, `cfg!`, and `cfg_attr` that mention `clippy` or `debug_assertions` are rejected. The scan walks every nested meta of `cfg_attr`, the predicate and the attributes it applies, and it walks `macro_rules!` bodies as tokens. A `$` splice does not hide `#[cfg(not(clippy))]` inside the macro. A plugin has no reason to branch on the build profile or on whether the compiler is Clippy.
- `allow`, `expect`, and `warn` on `clippy::disallowed_*`, `clippy::all`, `clippy::style`, or `clippy::restriction` are rejected, including when they sit inside `cfg_attr`. `clippy::disallowed_types` and `clippy::disallowed_methods` are `clippy::style` lints. `clippy::all` silences them under `-D`.
- Every plugin manifest sets `[lints] workspace = true`. The workspace forbids `unsafe_code`, which is what rejects an FFI block such as `extern "C" { fn system(); }`.
- A plugin must not have a `build.rs`. `BUILD_SCRIPT_ALLOW` in `xtask/src/boundary.rs` is the allow-list and it is empty. The scan also rejects `rustc-cfg` and `rustc-check-cfg` in a build script (`cargo:rustc-cfg`, `cargo::rustc-cfg`, and `cargo::rustc-check-cfg`), so a name added to that list later still cannot set a `cfg` that hides code from Clippy.
- Clippy then runs with `-F clippy::disallowed_types` and `-F clippy::disallowed_methods`, using `xtask/plugin-boundary/clippy.toml`. `-F` cannot be overridden in source. The config is applied after macro expansion, so `std::$m::Command` is `std::process::Command`. Profiles are `dev` and `release`. A package with no `[features]` is linted once per profile, on the default feature set (`--all-features` and `--no-default-features` compile that same crate). A package that defines features is linted three times per profile: the default set, `--all-features`, and `--no-default-features`. CI runs this step on Linux, macOS, and Windows, so a `cfg` for the host operating system of those runners is compiled.

## Known limits

- This is a review aid. A change that passes the gate can still spawn a process if the author writes ordinary Rust that the lints do not name.
- Clippy compiles the host triple of the CI runner. Arch-specific `cfg` for a target that runner is not building is a known limit. That includes non-dist targets such as `wasm32`, and dist triples the runners do not use (`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`).
- A `cfg(feature)` that is true only for a combination other than the default set, every feature, or no features is not compiled. The three sets above are the ones the gate lints.
- The source scan does not expand `macro_rules!`. It does reject `cfg` / `cfg_attr` / `cfg!` in a macro body that name `clippy` or `debug_assertions`. Clippy sees the expanded splice on the feature sets above. An allow-listed `build.rs` is not expanded either. None are allow-listed today, so no plugin build script runs.
- A proc-macro already in the API or SDK closure runs at compile time. That is the same trust as the rest of the workspace build.
- String-built command names and calls through function pointers are ordinary Rust. The gate does not try to interpret them.
