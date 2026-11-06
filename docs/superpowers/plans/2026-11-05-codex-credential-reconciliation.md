# Codex credential reconciliation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the Palmer-approved single-transaction Codex credential reconciliation protocol so refresh, adopt, and OAuth write-back stay fail-closed under lease → mutex → flock → cache lock, with in-blob markers and unique home ownership.

**Architecture:** Keep all protocol state inside the existing protected account-secret JSON (no new plaintext journals). Drive every refresh through `tokens::rotate` / `rotate_for_test` under account lease, then a composite home guard (`local mutex` + directory flock on the canonical Codex home inode), with step-0 CandidatePending repair before any OAuth POST. Keychain/auth.json reads for reconcile are always fresh under that guard.

**Tech Stack:** Rust workspace plugin `plugins/subscriptions/codex` (`hofund-plugin-subscription-codex`), plugin-host account lease/commit, existing `CodexLock` / `StoreReader` / Keychain helpers, `/workspace/hofund/bin/cargo-serial` tests.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-11-05-codex-credential-reconciliation-design.md` (approved 2026-11-05 ~21:40 ET).
- Lock order: lease (wait 30s) → local mutex → directory flock on codex_home inode → cache mutex (brief).
- Home composite wait: 10s total, 50ms retry; Busy/Io → release partial guards → retryable Busy with **zero** POST/write/commit/adopt.
- In-blob fields (exact keys are implementation detail but must cover): `refresh_attempt_id`, `refresh_canonical_home`, `refresh_store_location_fp`, `refresh_baseline_tuple_fp` (literal ABSENT when signed out), `refresh_candidate_tuple_fp`, candidate secrets, `refresh_invalid`.
- Marker precedence: at most one of InFlight-only, CandidatePending, INVALID; if both InFlight and Pending legacy → CandidatePending wins.
- Home owner: Codex-plugin-only `list()` + secret `codex_home` canonical compare under the same home composite guard after caller lease; 0 bind, 1 must equal lease AccountId, ≥2 fail-closed. No second account lease while home guard held. No other plugins, no new permissions.
- Ambiguous RefreshInFlight (crash/unknown HTTP/timeout) ⇒ reauth; never auto-retry refresh POST unless provably unsent.
- Never log secrets or fingerprint values; English only; no CJK; never write the reference project name outside allowlisted docs.
- Env: `CARGO_TARGET_DIR=/workspace/hofund/target-r-47b CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only`; every cargo via `/workspace/hofund/bin/cargo-serial`.
- Merge bar later: GATES_FAILED=0, ALL_KILLED (TIMEOUT not killed), green CI, merge origin/main (no rebase/force-push), independent security review, no release/tag.
- Do not weaken R-8a or other sensitive-feature holds.

## File map

| File | Responsibility |
|------|----------------|
| `plugins/subscriptions/codex/src/reconcile.rs` (new) | Marker helpers, fingerprints, step 0–5 OAuth protocol, Pending repair, Busy/conflict errors |
| `plugins/subscriptions/codex/src/tokens.rs` | Wire `access` / `rotate` / `rotate_for_test` through lease + reconcile protocol |
| `plugins/subscriptions/codex/src/agent_sign_in.rs` | Replace/narrow `reconcile_before_refresh` / `persist_refresh` to call reconcile module; owner bind on sign-in |
| `plugins/subscriptions/codex/src/codex_lock.rs` | Local mutex + flock composite; Busy on timeout; inode/path change abort; same-process Busy |
| `plugins/subscriptions/codex/src/store.rs` | Fresh Keychain reads for reconcile (`fresh: true` bypass positive+negative); Missing TTL; dual FakeKeychain isolation |
| `plugins/subscriptions/codex/src/discover.rs` | Owner scan helper used by reconcile (list + secret home compare); import path unchanged except owner rules |
| `plugins/subscriptions/codex/src/lib.rs` | `mod reconcile;` |
| `plugins/subscriptions/codex/src/reconcile_tests.rs` (new) or `#[cfg(test)]` in reconcile | M1–M15 regression matrix |
| `docs/feature-parity.md` | Document Codex reconcile behavior / remaining gaps only if needed |
| Spec status already updated in this branch | Approved header |

---

### Task 1: Marker + fingerprint types (no network)

**Files:**
- Create: `plugins/subscriptions/codex/src/reconcile.rs`
- Modify: `plugins/subscriptions/codex/src/lib.rs`
- Test: unit tests in `reconcile.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `hofund_plugin_sdk::SecretObject`, `SecretString`
- Produces: `RefreshMarkers`, `TupleFp`, `StoreLocationFp`, `parse_markers`, `with_inflight`, `with_pending`, `clear_markers`, `set_invalid`, `fingerprint_tuple`, `fingerprint_location`

- [ ] **Step 1: Write failing tests for marker precedence and ABSENT baseline**

```rust
#[test]
fn pending_wins_over_legacy_inflight() {
    let blob = SecretObject::new()
        .with_text("refresh_attempt_id", "a1")
        .with_text("refresh_candidate_tuple_fp", "cand");
    assert!(matches!(parse_markers(&blob), Markers::CandidatePending { .. }));
}

#[test]
fn baseline_absent_when_store_signed_out() {
    assert_eq!(fingerprint_tuple_absent(), "ABSENT");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/workspace/hofund/target-r-47b /workspace/hofund/bin/cargo-serial test -p hofund-plugin-subscription-codex pending_wins_over_legacy_inflight baseline_absent_when_store_signed_out`
Expected: FAIL (module / symbols missing)

- [ ] **Step 3: Implement marker helpers + fingerprints (hash full tuple; never log fps)**

Implement `parse_markers` returning `InFlight | CandidatePending | Invalid | None`. Fingerprints hash access+refresh+id_token+store account_id; location fp covers file vs keychain + mode. Candidate secrets written only at Pending step.

- [ ] **Step 4: Run tests to verify they pass**

Run: same command as Step 2. Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add plugins/subscriptions/codex/src/reconcile.rs plugins/subscriptions/codex/src/lib.rs
git commit -m "R-47b: add in-blob refresh markers and fingerprints"
```

---

### Task 2: Composite home guard (lock order under lease)

**Files:**
- Modify: `plugins/subscriptions/codex/src/codex_lock.rs`
- Test: `codex_lock.rs` tests + new same-process Busy / inode tests (M12)

**Interfaces:**
- Consumes: existing `CodexLock::acquire` flock behavior
- Produces: `HomeGuard` that takes process-local mutex keyed by canonical home, then flock; `try_acquire_home(home) -> Result<HomeGuard, Busy>`; drop releases both; Busy after 10s/50ms with zero side effects

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn same_process_second_taker_is_busy() {
    let (_dir, home) = lock_home("same-proc");
    let first = try_acquire_home(&home).await.expect("first");
    assert!(try_acquire_home(&home).await.is_err());
    drop(first);
}

#[tokio::test]
async fn lock_file_recreate_stays_busy_while_inode_held() {
    // Hold guard; delete/recreate .codex.lock; second process/taker still Busy (M12).
}
```

- [ ] **Step 2: Run to verify fail**

Run: `... test -p hofund-plugin-subscription-codex same_process_second_taker_is_busy`
Expected: FAIL

- [ ] **Step 3: Implement local mutex map + flock; Busy/Io release partial; alias paths use canonical home**

Canonical absolute path (symlink target) keys mutex and owner metadata. Inode/path change under guard → abort store write (checked in Task 4 write path).

- [ ] **Step 4: Tests PASS**

- [ ] **Step 5: Commit** `R-47b: composite Codex home guard with Busy timeout`

---

### Task 3: Unique home owner scan (Codex-only)

**Files:**
- Modify: `plugins/subscriptions/codex/src/discover.rs` and/or `reconcile.rs`
- Modify: `agent_sign_in.rs` upsert/sign-in bind path
- Test: M1 via `rotate_for_test`

**Interfaces:**
- Produces: `async fn home_owners(host, canonical_home) -> Result<Vec<AccountId>, PluginError>` using Codex `list()` + secret `codex_home` canonical compare only
- Produces: `fn assert_unique_owner(owners, lease_id) -> Result<(), PluginError>` (0 bind, 1 equal, ≥2 fail-closed)

- [ ] **Step 1: Failing M1 test — two accounts same home fail before POST**

```rust
#[tokio::test]
async fn m1_two_accounts_same_home_fail_before_post() {
    // Seed two accounts with same codex_home; rotate_for_test; assert Auth/Busy conflict and http().calls() empty.
}
```

- [ ] **Step 2: Run — expect FAIL**

- [ ] **Step 3: Implement owner scan under home guard after lease; bind writer publishes under same guard; no second lease**

If existing APIs cannot express this, **STOP and escalate Palmer** (spec rule).

- [ ] **Step 4: PASS**

- [ ] **Step 5: Commit** `R-47b: enforce unique Codex home owner under composite guard`

---

### Task 4: OAuth protocol steps 0–5 + Pending repair

**Files:**
- Modify: `reconcile.rs`, `tokens.rs`, `agent_sign_in.rs`
- Test: M2–M5, M8–M10 via `rotate_for_test` / `access`

**Interfaces:**
- Produces: `async fn reconcile_rotate(host, endpoints, account, stale, skip_keychain) -> Result<Access, PluginError>` implementing the ordered table
- Consumes: lease already held by caller OR takes lease internally exactly once first

Protocol (must match spec tables exactly):
0. CandidatePending repair under full guard + fresh read (match candidate / baseline|ABSENT / else conflict keep Pending, no OAuth same call)
1. commit RefreshInFlight + fps (0 POST on failure)
2. Single OAuth POST (guard held); definitive non-rotating issuer Auth may clear InFlight; unknown HTTP/timeout keeps marker → reauth; no auto-retry unless unsent
3. commit CandidatePending + candidate tuple (keep InFlight on failure semantics per spec)
4. Re-read store; baseline fp match → conditional write; third tuple → skip write, KEEP Pending, RETURN conflict, **no step 5**
5. commit clear markers + full tuple; failed clear retains Pending

Store-state table: SignedOut/ABSENT may OAuth after InFlight; deleted-after-baseline no; InvalidJson/Unreadable/Refused no; matching Need noop; stale Need OAuth after markers.

- [ ] **Step 1: Write failing tests M2 (happy path), M3 (clear-fail retains Pending), M4 (InFlight crash ⇒ reauth 0 POST retry), M5 (Pending repair no rollback), M8 (lock Busy zero side effects), M9 (no double POST), M10 (invalid/unreadable/refused 0 POST)**

- [ ] **Step 2: Run — expect FAIL**

- [ ] **Step 3: Replace `reconcile_before_refresh` / `persist_refresh` call sites in `tokens::rotate` with `reconcile_rotate`; keep detached job semantics (cancel wait-safe, durability continues)**

- [ ] **Step 4: PASS targeted tests; also run existing `tokens` / `agent_sign_in` / `codex_lock` tests

Run: `... test -p hofund-plugin-subscription-codex --lib`
Expected: PASS

- [ ] **Step 5: Commit** `R-47b: single-transaction OAuth reconcile with Pending repair`

---

### Task 5: Adopt & expired access (M6–M7)

**Files:**
- Modify: `reconcile.rs`, `tokens.rs`
- Test: M6 expired adopt → retryable refresh-needed 0 POST; M7 401 Replace + full tuple readopt

- [ ] **Step 1–4:** TDD as above; usable access commits full tuple success 0 POST; expired commits full tuple then retryable refresh-needed; Need::Replace compares current lease access; full tuple readopt on 401
- [ ] **Step 5: Commit** `R-47b: adopt usable and expired Codex store tuples`

---

### Task 6: Keychain cache freshness (M13) + redaction (M14)

**Files:**
- Modify: `store.rs` (`StoreReader`, cache TTL, FakeKeychain isolation)
- Test: Missing post-TTL live find; dual FakeKeychain same home isolated; error messages never contain tokens/fps

- [ ] **Step 1–4:** TDD
- [ ] **Step 5: Commit** `R-47b: fresh Keychain reads for reconcile and cache isolation`

---

### Task 7: Home safety + macOS/Linux lock matrix (M11–M12 remainder)

**Files:**
- Modify: `store::ensure_codex_home`, write-back path
- Test: unsafe parent rejected; new home 0700 only; do not chmod existing safe home; alias paths share lock; inode change aborts write

- [ ] **Step 1–4:** TDD
- [ ] **Step 5: Commit** `R-47b: Codex home safety and inode-stable flock`

---

### Task 8: Wire agent_sign_in / discover entrypoints + docs

**Files:**
- Modify: `agent_sign_in.rs`, `discover.rs`, `docs/feature-parity.md` (allowlisted) if behavior changes need a row
- Test: production paths through `write` / import still call owner+guard rules; no new permissions

- [ ] **Step 1–4:** Integration tests for sign-in bind and discover import under owner rules
- [ ] **Step 5: Commit** `R-47b: apply reconcile owner rules to sign-in and discover`

---

### Task 9: Full package verify + supervisor handoff

- [ ] **Step 1:** Run bounded pipeline twice:

```bash
export CARGO_TARGET_DIR=/workspace/hofund/target-r-47b CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
/workspace/hofund/bin/cargo-serial fmt --all -- --check
/workspace/hofund/bin/cargo-serial clippy -p hofund-plugin-subscription-codex --all-targets --all-features -- -D warnings
/workspace/hofund/bin/cargo-serial test -p hofund-plugin-subscription-codex --all-features
/workspace/hofund/bin/cargo-serial test -p hofund-plugin-subscription-codex --all-features
```

Expected: all exit 0. Save to `/workspace/hofund-review/diagnostics/r-47b-plan-verify.log`.

- [ ] **Step 2:** Map every M1–M15 row to a named test; list any STOP/escalate items.
- [ ] **Step 3:** Do **not** push. Hand off to a **fresh** implementation context and a **separate** read-only adversarial reviewer ≠ author; supervisor full-diff; then unchanged full gates + mutation; merge only after independent security review.

---

## Self-review (plan vs spec)

1. Spec coverage: in-blob fields ✓; unique home owner ✓; lock order/Busy ✓; store-state table ✓; OAuth 0–5 + Pending repair ✓; adopt/expired ✓; Keychain cache ✓; fail-closed matrix ✓; M1–M15 ✓; post-approval merge bar ✓.
2. Placeholder scan: no TBD/TODO; concrete files and test names.
3. Type consistency: `HomeGuard`, `RefreshMarkers` / `Markers`, `reconcile_rotate`, `home_owners` used consistently across tasks.
