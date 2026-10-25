# Releasing hofund

Version 0.1.0 is prepared in the tree: the workspace version, this process,
and the changelog. Pushing a tag publishes a GitHub release. Do not push a
tag until the changelog and the version agree.

The CLI and the menu-bar app are two workflows. Both run on a tag that looks
like `v0.1.0`. Neither of those publish workflows runs on a pull request.
`.github/workflows/release-dry-run.yml` does. It builds artifacts and
checksums and does not create a GitHub release.

## Version

1. Set `version` under `[workspace.package]` in `Cargo.toml`, and set the same
   version on every `hofund-*` entry in `[workspace.dependencies]`.
2. Set the same version in `xtask/tests/workspace_invariants.rs`.
3. Move notes from `[Unreleased]` in `CHANGELOG.md` to `[x.y.z] - YYYY-MM-DD`.
4. Commit those files. The tag is created from that commit, not from a dirty tree.

The crates stay `publish = false`. This process does not publish to crates.io.

## CLI

`dist-workspace.toml` configures cargo-dist 0.32.0. It builds the `hofund`
binary only (`hofund-desktop` and `xtask` set `dist = false`).

`dist build` passes `--profile dist`. Cargo rejects that profile unless
`Cargo.toml` defines it. `dist init` writes:

```toml
# The profile that 'dist' will build with
[profile.dist]
inherits = "release"
lto = "thin"
```

`dist plan` does not compile with that profile, so a green plan is not a
build. The dry-run below runs `dist build`.

Targets cargo-dist accepts:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`

cargo-dist 0.32 rejects `universal2-apple-darwin`. The release workflow adds
that archive itself: after the two macOS builds finish, `universal-macos`
runs `lipo`, writes `hofund-cli-universal2-apple-darwin.tar.gz`, and writes
a `.sha256` sidecar in the same `sha256sum` layout as the other archives.
The shell and PowerShell installers still select one architecture. The
universal archive is an extra asset on the GitHub release.

`macos-sign` stays false. cargo-dist treats an empty string as a certificate
and tries to decode it, and its codesign invocation does not pass a
timestamp. Signing is a later step in the workflow, and only when the secret
group is non-empty.

Installers are `hofund-cli-installer.sh` and `hofund-cli-installer.ps1`.
Each archive has a SHA-256 checksum, and the global job also writes
`sha256.sum`.

`.github/workflows/release.yml` is generated, then adjusted. Refresh it
from the current dist config before editing:

```sh
# dist generate refuses to write CI while allow-dirty lists "ci".
# Drop that one line, generate, then put the line back.
dist generate --mode ci
```

`dist init --yes` is what wrote `[profile.dist]` and the current
`dist-workspace.toml` keys, including `install-path = "CARGO_HOME"` (the
default dist inlines for the shell and PowerShell installers). `dist
generate` overwrites the adjustments below. After generating, restore all of
these:

- the `Install Linux host libraries` step (dist builds the whole workspace,
  including the menu-bar crate, and Linux needs those libraries to compile it)
- the `Build artifacts` step calls `.github/scripts/repack-macos-cli.sh`
- the `Sign Windows archives` step calls `.github/scripts/sign-windows-cli.ps1`
- the `universal-macos` job
- `universal-macos` on the `host` job `needs` list, and in that job's `if`
- the `Download the universal archive` step in `host`
- the `Stage the universal archive` step in `host`

`allow-dirty = ["ci"]` is set because of those edits. cargo-dist 0.32 treats
a `release.yml` that differs from `dist generate` as an error, and the tag
workflow stops in `dist host --steps=create` before it creates a GitHub
release. With `ci` in that list, dist leaves the file alone. The adjustments
above still have to be restored after `dist generate`; the setting does not
put them back.

Leave `macos-sign = false`. Turning it on puts `CODESIGN_*` into the `dist
build` environment, including when the secrets are empty.

The release workflow runs only when a version tag is pushed. It does not run
on pull requests (`pr-run-mode = "skip"`). CI runs `dist plan` on every pull
request, using the `cargo-dist-version` in `dist-workspace.toml`. That is the
same integrity check the tag's plan job runs. The CI job also requires the
adjustments above to still be present in `release.yml`, requires
`[profile.dist]`, and checks that `release.yml` is still `dist generate`
plus `.github/dist-release.patch`.

## Dry-run

`.github/workflows/release-dry-run.yml` runs `dist build --artifacts=local`
and the desktop bundle and checksum jobs. It does not pass a tag to `dist
host` and it does not call `gh release`. The token is read-only.

A pull request runs it when the change touches a release workflow, a release
script, `dist-workspace.toml`, `Cargo.toml`, `Cargo.lock`, `apps/desktop`,
or `rust-toolchain.toml`. That pull-request run builds
`x86_64-unknown-linux-gnu` only, plus the desktop bundles on macOS, Linux,
and Windows.

`workflow_dispatch` builds every local target from `dist plan`
(`aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`) and the same three
desktop jobs. Trigger it on the branch you are about to tag.

## Menu bar

`.github/workflows/release-desktop.yml` builds `hofund-desktop` through
`.github/actions/desktop-bundle`. That action installs `@tauri-apps/cli`
pinned to 2.12.4. The dry-run uses the same action.

The Rust crate `tauri` is 2.12.5. The CLI package does not publish 2.12.5.
2.12.4 is the CLI released with that crate on 2026-10-21 (`tauri-cli` 2.12.4
on crates.io, `@tauri-apps/cli` 2.12.4 on npm). Installing a missing version
fails with npm `ETARGET` before any bundle is built. CI installs this same
pin on every pull request and checks `tauri --version`.

| Runner | Bundles |
| --- | --- |
| `macos-latest` | `dmg`, `app` |
| `ubuntu-latest` | `deb`, `appimage` |
| `windows-latest` | `nsis`, `msi` |

Linux needs WebKitGTK 4.1, Ayatana AppIndicator, librsvg, patchelf, and
libxdo. The workflow installs them. The job writes `SHA256SUMS` next to the
bundles and uploads them to the GitHub release for that tag after the CLI
workflow has created it.

The bundle directory comes from `cargo metadata`'s `target_directory`, plus
`release/bundle` (`.github/scripts/desktop-bundle-dir.sh`). `hofund-desktop`
is a workspace member, so that directory is the workspace target directory,
including when `CARGO_TARGET_DIR` is set. Checksums and Windows signing both
use it. The package directory's own `target/` is not the one Cargo writes.

## Signing and notarization

A secret group is used only when every value in the group is non-empty. A
missing secret prints that the group was skipped and does not fail the job.
A present secret that the platform rejects does fail the job, because the
release was asked to sign.

Apple code signing for the CLI and the app bundle:

| Secret | Value |
| --- | --- |
| `CODESIGN_CERTIFICATE` | Base64 of a Developer ID `.p12` |
| `CODESIGN_CERTIFICATE_PASSWORD` | Password for that `.p12` |
| `CODESIGN_IDENTITY` | Signing identity, such as `Developer ID Application: Example (TEAMID)` |

The CLI job signs each macOS `hofund` with `--options runtime --timestamp`,
rebuilds the `.tar.gz`, and rewrites that archive's `.sha256` plus the hash
stored in `dist-manifest.json`. The installer job reads the manifest, so the
published checksum is the hash of the signed bytes. The universal archive is
signed the same way after `lipo`. The desktop job maps the certificate
secrets onto `APPLE_CERTIFICATE` and `APPLE_CERTIFICATE_PASSWORD`, which is
what the Tauri bundler reads, and leaves them unset when either is empty.

Notarization runs after a signed macOS artifact exists. It needs all three of:

| Secret | Value |
| --- | --- |
| `APPLE_ID` | Apple ID email |
| `APPLE_PASSWORD` | App-specific password |
| `APPLE_TEAM_ID` | Team id |

The desktop job lets `tauri build` submit and staple when those three are
set, and unsets them when any one is empty. The CLI job submits a zip copy
of the signed binary with `notarytool --wait`. That copy is not the published
archive, and the command does not staple a raw binary, so the `.tar.gz` and
its checksum stay as they were after signing. Notarization is skipped when
the signing secrets are absent.

Windows Authenticode uses:

| Secret | Value |
| --- | --- |
| `WINDOWS_CERTIFICATE` | Base64 of a `.pfx` code-signing certificate |
| `WINDOWS_CERTIFICATE_PASSWORD` | Password for that `.pfx` |

Both jobs import the certificate and run `signtool` (`/fd SHA256`, RFC 3161
timestamp). The desktop job signs the `.exe` and `.msi`, then writes
`SHA256SUMS`. The CLI job signs `hofund.exe`, rebuilds the flat zip, and
rewrites that zip's `.sha256` and the manifest hash before the installer job.

## Local build

```sh
cargo install --path crates/cli --locked
cargo build -p hofund-desktop
```

On Linux, install the libraries named above before building the menu-bar app.
`cargo test --workspace` builds it too. The CI workflow installs those
libraries on the Linux runners.
