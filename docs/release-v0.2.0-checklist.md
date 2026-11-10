# v0.2.0 release checklist (Must #12)

This is a runbook for cutting **v0.2.0**. It does **not** itself bump any
crate or app version, create or push a tag, publish a GitHub Release, merge
a pull request, or run live-account or live-network smoke.

The general tag, dry-run, desktop, and signing process is
[docs/release.md](release.md). Follow that file when the cut actually
happens. This page is the v0.2.0-specific preflight: what is still
unmerged, what a dry-run must prove, and which smoke stays with Palmer.

The reference freeze for this release is magpie `v0.1.3005` (`6abacd46`).
hofund `main` at the time of writing is `318912ff` (R-6: Claude Code bridge
design, #736). Workspace version strings still say `0.1.0`. **v0.2.0 is not
tagged, and no GitHub Release has been published from this tree.**

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

## Honest preflight (2026-11-11)

Scores and user-facing honesty for what is on `main` versus open work are
the subject of Must #11,
[PR #776](https://github.com/logibricks-io/open-heimdall-hofund/pull/776)
(`feat/must11-docs-honesty`, head `5a93c335`). That pull request is
**open and unmerged**. The tagged tree should include it, or equivalent
wording. Until it lands, `docs/feature-parity.md` on `main` still has a
stale Summary and an older snapshot header.

Most subscription and routing work is verified with mocks only.

### Already on `main` (do not redo)

| Must item | State on `318912ff` |
| --- | --- |
| Usage reports 9.4 / 9.9 | Merged (#665) |
| D-71, D-73, D-74, D-75 | Merged (R-4) |
| Phase-3 A-2c | Merged (#611) |
| Phase-3 A-2d lapsed-team refresh | On `main` (`a2d_24`) |
| Claude account-dir cleanup (#303) | Closed, on `main` |
| Zed POST `/completions` (R-16) | On `main`. This is not login-and-quota-only. |
| Claude Code request serving | **Not** on `main`. `upstream()` returns unsupported. Design only (#736). |
| Cursor agent-API request serving | **Not** on `main`. `upstream()` returns unsupported. |
| Devin chat | **Not** implemented. The frozen must-list puts this in Next (§2.2), not Must. |
| Kiro | Unary text-only on `main` (no stream, tools, or images). That is the Must scope. |
| Antigravity (row 3.4) | No plugin. Not a Must item. |

### Open must-list work (Palmer merges; this runbook does not)

These are open at the time of writing. Heads will move if CI fixes are
merged forward. Re-check `gh pr view` before the cut. None of this is
shipped until it is on `main`.

| PR | Topic | Branch / last known head |
| --- | --- | --- |
| #746 | D-84 Codex compaction fallback | `feat/r20-codex-compaction-fallback` |
| #747 | D-80 Codex 0.166+ thread relabel | `feat/r19-codex-thread-relabel` |
| #748 | P1-8 uninstall / purge | `feat/r21-uninstall` |
| #749 | 10.5 LAN allow-IPs and trusted proxy | `feat/r22-lan-allow-ips` |
| #750 | 15.1 tray status, route, quota | `feat/r23-tray-status` |
| #751–#752 | R-25 stability (#563, #463, #612, #465, #607 follow-ups) | `feat/r25a-stability-b`, `feat/r25b-stability-b` |
| #753–#762 | Claude H8b request serving | stack on `feat/r6a-bridge-callback` … `feat/r6g-errors-docs` |
| #763–#775 | Cursor R-8/R-9 agent-API request serving | stack up to `feat/r9e-images` @ `0423cbec` |
| #776 | Must #11 documentation honesty + #274 | `feat/must11-docs-honesty` @ `5a93c335` |
| (this PR) | Must #12 checklist only | `feat/must12-release-checklist` |

Related issues that stay open after this PR: #609, #607, #612, #465, #463,
#563 (implementation is in the stacks above; do not treat the issues as
done until the matching PRs merge).

### Not in this cut unless Palmer expands the freeze

- Devin GetChatMessage / chat upstream (design in
  [docs/devin-upstream-design.md](devin-upstream-design.md); §2.2 Next).
- Other §2.2 Next items (auxiliary rows, D-46 cluster, and so on).
- D-50 stays skipped.
- R-47b stays held (`838a5338` / `a3183edc`).
- 15.3 TUI is [PR #740](https://github.com/logibricks-io/open-heimdall-hofund/pull/740).
  Palmer chooses whether it is in v0.2.0 or deferred. This runbook does not
  merge it.

Dependabot PRs (#741–#745 and later) are not must-list items. Include them
only if Palmer wants the bumps on the tagged commit.

## A. Merge preflight (Palmer)

1. Decide, in writing, which of the open must-list PRs above land in
   v0.2.0 and whether #740 is in or deferred.
2. Merge those pull requests to `main` yourself. Agents must not merge to
   `main`.
3. Confirm `origin/main` contains Must #11 (#776) or equivalent honesty
   text, so the tagged docs do not claim unmerged request serving as
   released.
4. Confirm CI is green on the commit that will be tagged. For the Cursor
   stack, a known-flake rerun is allowed only for macOS `a3a_26` /
   `a3a_27`, Windows `control_picks`, plugin-host `r4_72` variants, and
   “Install desktop host libraries” timeouts. `delayed_http2` and the
   heartbeats tool-count wait are **not** known infra flakes; they need a
   real fix before the cut.
5. Do not bump versions until this preflight is done.

## B. Version and changelog (do not perform until the cut)

When Palmer is ready, follow [docs/release.md](release.md) § Version on
the to-be-tagged commit, with **0.2.0**:

1. Set `version` under `[workspace.package]` in `Cargo.toml`, and the same
   version on every `hofund-*` entry in `[workspace.dependencies]`.
2. Set the same version in `xtask/tests/workspace_invariants.rs`.
3. Move notes from `[Unreleased]` in `CHANGELOG.md` to
   `[0.2.0] - YYYY-MM-DD`. Be honest: do not list Cursor or Claude Code
   request serving, or Devin chat, as released unless the matching PRs are
   already on that commit. Keep Kiro as unary text-only unless a later
   change landed.
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
| #304 | Real `claude` binary: `claude auth login --claudeai` non-TTY output and `claude auth status --json` shape, plus sign-in and refresh of the hofund Claude subscription | Scripted-child tests are not a substitute. H8b request serving, if merged, still needs this smoke before it is claimed as verified against a real binary. |
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
