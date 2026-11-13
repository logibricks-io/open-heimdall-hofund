# v0.2.0 release checklist (Must #12)

This is a runbook for cutting **v0.2.0**. It does **not** itself bump any
crate or app version, create or push a tag, publish a GitHub Release, merge
a pull request, or run live-account or live-network smoke.

The general tag, dry-run, desktop, and signing process is
[docs/release.md](release.md). Follow that file when the cut actually
happens. This page is the v0.2.0-specific preflight: what still remains
to cut the tag, what a dry-run must prove, and which smoke stays with Palmer.

The reference freeze for this release is the inspiration project's `v0.1.3005` (`6abacd46`).
hofund `main` at the time of writing is `f320d119` (Must #11 documentation
honesty, #776, 2026-11-13, which sits on R-34 `74dbebbc`). Workspace version
strings still say `0.1.0`. **v0.2.0 is not tagged, and no GitHub Release has
been published from this tree.**

## What this pull request does not do

- It does not change `[workspace.package] version` or any `hofund-*`
  dependency version.
- It does not move `CHANGELOG.md` notes out of `[Unreleased]`.
- It does not create or push `v0.2.0`, and it does not dispatch a tag
  workflow.
- It does not merge any pull request, including this one, to `main`.
- It does not run, and must not be read as having run, real-account smoke
  (#58, #304, #310). Those stay Palmer's. This document does not close
  those issues.

## Honest preflight (2026-11-13)

Scores and user-facing honesty for what is on `main` landed as Must #11
([PR #776](https://github.com/logibricks-io/open-heimdall-hofund/pull/776),
squash `f320d119`). The tagged docs must keep matching the landed surface;
this checklist does not replace that invariant.

Most subscription and routing work is verified with mocks only.

### Already on `main` (do not redo)

| Must item | State on `f320d119` (2026-11-13) |
| --- | --- |
| Usage reports 9.4 / 9.9 | Merged (#665) |
| D-71, D-73, D-74, D-75 | Merged (R-4) |
| Phase-3 A-2c | Merged (#611) |
| Phase-3 A-2d lapsed-team refresh | On `main` (`a2d_24`) |
| Claude account-dir cleanup (#303) | Closed, on `main` |
| Zed POST `/completions` (R-16) and RSA sign-in (R-56) | On `main`. This is not login-and-quota-only. |
| Claude Code request serving (H8) | Merged (#762 tip, including R-6g). Mocks only; real-binary smoke is still #304. |
| Cursor agent-API request serving (R-8/R-9) | Merged (#775 tip). Mocks only; documented deviations stay in `docs/feature-parity.md`. |
| Devin chat (R-12/R-13) | Merged (#786 tip). Mocks only. |
| Kiro | Unary or streaming Messages with tools and inline images (R-51, mocks/loopback only). Portal/browser sign-in, kiro-cli SQLite import, delegated refresh and compare-and-swap write-back, and external-idp refresh are included. Images over the 5 MiB Base64 limit are refused. Live-account verification, deterministic image shrinking, native effort/reasoning, and the other documented gaps remain open. |
| Antigravity (row 3.4) | Plugin is on `main` (R-49). Mocks only. |
| P1-8 uninstall / purge | Merged (#748) |
| 10.5 LAN allow-IPs / trusted proxy | Merged (#749) |
| 15.1 tray status, route, quota | Merged (#750) |
| D-80 Codex thread relabel | Merged (#747) |
| D-84 Codex compaction fallback | Merged (#746) |
| R-25 stability follow-ups | Merged (#751, #752) |
| 15.3 TUI Providers / Agents | Merged (#740) |
| Sessions R-29–R-34 and TUI Sessions (R-44) | Merged (#792, #794–#799). Library writes and true 4.2–4.4 recording/MCP execution are not on `main`. |
| #776 Must #11 documentation honesty + #274 | Merged (`f320d119`). Snapshot counts and user-facing docs match the landed surface. |

### Still open or unstarted at the cut (re-check before tagging)

| Item | State |
| --- | --- |
| #777 (this PR) Must #12 checklist only | Open docs PR; does not bump or tag. |
| Library writes (row 6 / R-35+) | No open implementation PR. Palmer included Library in the v0.2.0 scope; it is unstarted. |
| True 4.2–4.4 session recording / capture, library mutate, MCP execution | Held. |
| R-47b Codex store | Held (`838a5338` / `a3183edc`). Awaiting Palmer's written spec approval. |
| Dependabot #741–#745 | Held out of the v0.2.0 land order unless Palmer includes them. |
| Version bump, tag, GitHub Release | Not started. Follow § B and § E only at the cut. |
| Real-account smoke #58, #304, #310 | Palmer only. Not on the agent critical path. |

Mid-stack PRs whose tips already landed (#753–#761, #763–#774, #779–#780, #784–#785, #788–#789, and similar) should not be merged again onto `main`.

### Not in this cut unless Palmer expands the freeze further

- Other §2.2 Next items (D-46 cluster, remaining auxiliary rows, and so on).
- D-50 stays skipped.
- True 4.2–4.4 implementation and R-47b stay held.

## A. Merge preflight (Palmer)

1. Decide, in writing, whether any remaining open or unstarted items
   above (especially Library R-35+ and held work) land in v0.2.0 or are
   deferred.
2. Confirm `origin/main` is the commit you intend to tag, and that Must
   #11 (#776) or equivalent honesty text is on that commit so the tagged
   docs match the landed surface.
3. Confirm CI is green on the commit that will be tagged. A known-flake
   rerun is allowed only for confirmed flakes on main-identical tests
   (for example the historical `free_port()` / `HOFUND_ADDR` race, which
   now has a deterministic fix in #808). `delayed_http2` and the
   heartbeats tool-count wait are **not** known infra flakes; they need a
   real fix before the cut.
4. Do not bump versions until this preflight is done.

## B. Version and changelog (do not perform until the cut)

When Palmer is ready, follow [docs/release.md](release.md) § Version on
the to-be-tagged commit, with **0.2.0**:

1. Set `version` under `[workspace.package]` in `Cargo.toml`, and the same
   version on every `hofund-*` entry in `[workspace.dependencies]`.
2. Set the same version in `xtask/tests/workspace_invariants.rs`.
3. Move notes from `[Unreleased]` in `CHANGELOG.md` to
   `[0.2.0] - YYYY-MM-DD`. Be honest about what is on that commit: Cursor
   R-8/R-9, Claude H8, and Devin R-12/R-13 request serving are already on
   `f320d119` (mocks only). If R-51 has landed, describe unary or streaming
   Messages with tools and images, the 5 MiB Base64 refusal, portal/browser
   sign-in, kiro-cli SQLite import, delegated refresh and compare-and-swap
   write-back, and external-idp refresh. Live-account verification,
   deterministic image shrinking, native effort/reasoning, and the other
   documented Kiro gaps remain open. Do not list Library writes or true
   4.2–4.4 recording/MCP execution as released.
4. Commit those files. The tag is created from that commit, not from a
   dirty tree.

The crates stay `publish = false`. This process does not publish to
crates.io.

## C. Release dry-run, including Windows (#357)

[docs/release.md](release.md) § Dry-run is the procedure. A pull-request
dry-run builds **Linux x86_64 only**, plus desktop bundles on macOS,
Linux, and Windows. That is **not** enough for the cut.

On the to-be-tagged commit, Palmer (or a maintainer) should:

1. Run `.github/workflows/release-dry-run.yml` via `workflow_dispatch` so
   every local cargo-dist target is built:
   `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`,
   `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
   `x86_64-pc-windows-msvc`, plus the three desktop jobs.
2. Confirm each `dist-build` job unpacks the CLI and daemon archives side
   by side, and that the runner's own target prints
   `hofund serve started the hofundd beside it`
   (`.github/scripts/check-two-binary-archives.sh`).
3. Confirm the Windows job in particular. #357 is still **open**: the
   script is invoked with `shell: bash` and is supposed to run
   `hofund.exe serve --config <missing>` when the archive target equals
   `rustc`'s host, looking for a `hofundd: ` prefix on stderr. The issue
   records that the Windows zip has historically been checked for contents
   without executing that serve path. A green Linux-only PR dry-run does
   not close #357. If the dispatched Windows job still skips the serve
   check, add the PowerShell step described in #357
   (`hofund.exe serve --config <missing>` must report the `hofundd: `
   prefix) and re-run. This checklist does not implement that step and
   does not dispatch the workflow.
4. Desktop dry-run jobs must produce the bundles in
   [docs/release.md](release.md) § Menu bar (`dmg`/`app`, `deb`/`appimage`,
   `nsis`/`msi`) and `SHA256SUMS`. A timeout on “Install desktop host
   libraries” may be retried; a bundler or pin failure may not.

## D. Real-account smoke (Palmer only)

Do **not** run these from an agent session, CI, or this PR. They need
Palmer present and real credentials. Record the outcome in
[docs/feature-parity.md](feature-parity.md) (and the subscription coverage
inventory if a row changes). Leave the issues open until that record
exists.

| Issue | What to run | Notes |
| --- | --- | --- |
| #304 | Real `claude` binary: `claude auth login --claudeai` non-TTY output and `claude auth status --json` shape, plus sign-in and refresh of the hofund Claude subscription | Scripted-child tests are not a substitute. H8 request serving is on `main` but still needs this smoke before it is claimed as verified against a real binary. |
| #310 | Live GitHub Copilot: device sign-in, token exchange, refresh, and a chat call, and the same against a GitHub Enterprise host | H6 is mocks only on `main`. |
| #58 | Live Vertex ADC and impersonation, and Bedrock Converse with a session token | Unit tests and wiremock are not a substitute. Opt-in / environment-gated is acceptable; it must actually have been run. |

Other subscriptions also lack real-account smoke. They are not separately
issues in the must-list, but they must not be described as live-verified
at tag time.

## E. Tag and GitHub Release (do not perform until A–D agree)

Only after the version and the changelog agree, CI is green, the
`workflow_dispatch` dry-run (including the Windows serve check) has
passed, and Palmer has either completed the smoke in § D or explicitly
accepted shipping with those issues still open:

1. Tag `v0.2.0` on that commit, as in [docs/release.md](release.md).
   Pushing the tag publishes the GitHub Release. Do not push a tag from
   this PR or from a dirty tree.
2. Confirm the CLI archives, universal macOS archive, Windows zip, both
   installer pairs, desktop bundles, and checksums appear on the release,
   as described in [docs/release.md](release.md).
3. Signing and notarization stay optional when secrets are absent. A
   present secret that the platform rejects fails the job; that is not a
   flake.

## After the tag

- Close #357 only if the dispatched Windows dry-run actually executed
  `hofund.exe serve` and saw the `hofundd: ` prefix.
- Close #58, #304, and #310 only if the live runs were recorded. Do not
  close them from this checklist.
- If some must-list PRs were deferred, say so in the GitHub Release notes
  and in the honesty docs. Do not imply they shipped.
