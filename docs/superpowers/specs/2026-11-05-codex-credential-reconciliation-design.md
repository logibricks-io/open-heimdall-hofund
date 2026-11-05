# R-47b: Codex credential reconciliation transaction (design spec)

**Status:** Draft for Palmer / supervisor written approval — **no implementation** until approved.

**Branch context:** `sup/r-47b-codex-store`; prior passes r1–r3 and Grok rr3 (`/workspace/hofund-review/r-47b-grok-rr3.md`) document six production defects around split locking, partial adopt, unsafe writes, and Keychain cache after deletion.

---

## Review assumptions (explicit; not open questions)

1. **Scope is Codex subscription plugin only** (`plugins/subscriptions/codex`): refresh, agent sign-in write-back, discover/import read paths that touch the same credential store — not gateway, router, or other subscriptions.
2. **Allowed durable stores** remain: hofund account secret (lease commit), Codex `auth.json`, macOS Keychain item keyed by stable home-derived account id, existing `config.toml` store mode — no new files, journals, or credential APIs.
3. **Production entrypoints** for regression are unchanged: `tokens::access` / internal `rotate`, `discover::list` / `discover::import`, `agent_sign_in::write`, and test hook `tokens::rotate_for_test` (must mirror production lock/reconcile ordering).
4. **Account lease** (`HostContext::accounts().lease` / `commit`) stays the sole writer to hofund account secrets; reconciliation may read store under lock but commits only through an held lease.
5. **OAuth token endpoint** is external; success means a new access (and usually refresh) in memory — not atomic with local disk or Keychain.
6. **R-8a merge ordering** and **restriction holds** stay: this spec does not relax CI, `ALL_KILLED`, timeout-failure gates, or merge-before-R-8a policy.
7. **Optional rr3 follow-ups** (account_id precedence in import, chmod on existing home, `security -w` argv) are **out of scope** unless listed below as non-goals.

---

## Bounded scope and non-goals

**In scope**

- One **credential reconciliation transaction** (state machine) per `(AccountId, codex_home path)` refresh or equivalent mutate path, owning:
  - store read + compare (file / Keychain / mode from `config.toml`),
  - decision: OAuth refresh vs adopt external store vs no-op,
  - ordered persistence to account lease and Codex store,
  - lock lifetime for in-process and cross-process writers.

**Non-goals**

- New sign-in flows, subscription features, plaintext secret logs, or secondary credential databases.
- Perfect atomicity across OAuth HTTP, Keychain, and account store (see **Recovery**).
- Fixing unrelated import/discover UX except where the same `StoreReader` / cache / fail-closed rules apply.
- Tag, release, or merge without independent security signoff (see **Security review**).

---

## Alternatives (2–3) and recommendation

| | **A. Single reconciliation transaction (recommended)** | **B. Narrow patch #4 on current split functions** | **C. OAuth-only in daemon; plugin read-only store** |
|---|--------------------------------------------------------|-----------------------------------------------------|-----------------------------------------------------|
| Idea | Replace `reconcile_before_refresh` + unguarded POST + `persist_refresh` with one state machine and one lock guard type held for the whole critical section. | Extend rr3 fixes (cache Missing, lease on Adopt, widen lock) without restructuring call graph. | Move refresh POST to daemon; Codex plugin only mirrors store. |
| Correctness | Fixes root cause: one ordering story for lock, compare, OAuth, commit, write-back. | High risk of a fourth pass missing a gap (rr3 already failed twice on lock span). | Violates current plugin-host boundary and duplicates OAuth in daemon. |
| Testability | One matrix maps states → assertions on POST count, lease secret, file/Keychain bytes. | Tests stay scattered across three modules. | Large architecture drift; out of R-47b budget. |
| **Recommend** | **Yes** — matches architecture decision memo option A. | No — systematic-debugging stop rule. | No — scope explosion. |

---

## Canonical identities

| Identity | Definition | Use |
|----------|------------|-----|
| **codex_home** | Resolved absolute path stored in account blob `codex_home` (agent sign-in) or discover/import home argument | Lock key, `ensure_codex_home` target, Keychain account derivation (`keychain_account(home)`) |
| **store account id** | `tokens.account_id` inside auth JSON when present | Upstream ChatGPT workspace selection after adopt/refresh |
| **hofund AccountId** | Plugin account row id | Lease key only |
| **Keychain cache key** | Stable string from home (existing `keychain_account`) | Process-global cache; tests must use isolated `FakeKeychain` + home paths so cache entries do not cross tests |

Reconciliation must treat **store access + refresh + store account id** as one logical credential set when adopting or after OAuth.

---

## Lock hierarchy and lifetime

**Layers ( acquire order )**

1. **Account lease** — existing `LEASE_WAIT` (30s); held for entire reconciliation including OAuth and all commits (unchanged outer guard).
2. **Codex home reconciliation lock** — new composite guard replacing ad hoc double `acquire`:
   - **Cross-process:** keep directory `flock` on `codex_home` (inode-stable; survives `.codex.lock` file recreate).
   - **In-process:** mutex keyed by canonical `codex_home` path so second `try_take` in the same process returns **Busy** on Linux and macOS (fixes macOS same-process `flock` reentrancy).
3. **Keychain cache mutex** — existing global lock; only for cache struct, never held across OAuth HTTP.

**Lifetime rule:** From first store read for compare until **both** (a) account `lease.commit` when reconciliation mutates secrets and (b) Codex store write-back attempt finishes or is skipped with explicit reason — the reconciliation lock **must** remain held. OAuth POST runs **inside** this span. Dropping the lock mid-POST is forbidden.

**Lock unavailable (`acquire` timeout → none)**

- Do **not** blind POST. Re-read store (without holding cross-process lock if timeout, but still under account lease): if access ≠ stale account secret → **Adopt path** (commit lease from store, zero POST). If still stale and store is **SignedOut** only → may POST once lease already held (see fail-closed). If store **InvalidJson** / **Unreadable** / unsafe home → **fail closed**, no POST.

**Cancellation**

- Detached refresh job (`codex-refresh`): if OAuth succeeded and `lease.commit` completed, write-back must still run (existing intent). If cancelled before commit, account secret unchanged; store may lag — next refresh reconciles. No partial OAuth token written to disk without commit.

**Bounded waits**

- Cross-process lock: keep 10s wait / 50ms retry; timeout behavior above.
- Lease: 30s unchanged.

---

## State machine (single owner)

States are internal to the reconciliation module; callers invoke one entry (e.g. `reconcile_refresh`) instead of separate reconcile + rotate body + persist.

```text
                    ┌──────────────┐
                    │   Start      │
                    │ (lease held) │
                    └──────┬───────┘
                           │ take reconciliation lock (or timeout path)
                           ▼
                    ┌──────────────┐
              ┌─────│ ReadStore    │───── fail-closed: InvalidJson,
              │     │ + compare    │       Unreadable, unsafe home
              │     └──────┬───────┘
              │            │
              │     access ≠ stale secret
              │            ├──────────────────► Adopt: build SecretObject
              │            │                    from store (access, refresh,
              │            │                    id_token, account_id); commit
              │            │                    lease; release lock; Done (0 POST)
              │            │
              │     access = stale, SignedOut
              │            ├──────────────────► OAuthRefresh ──► PersistOrdered
              │            │
              │     access = stale, valid store
              │            └──────────────────► OAuthRefresh ──► PersistOrdered
              │
              └─ timeout: re-read without lock; same decisions, no overwrite
```

**OAuth vs adopt**

- **Adopt:** Store already has a different access than account lease; load full token set from store; **`lease.commit`** with access, refresh, id_token, account_id, `codex_home`; return access to caller; **no OAuth POST**.
- **OAuth refresh:** Only when compare says stale matches account **and** store state is **SignedOut** or readable valid JSON with matching stale access; POST; then **PersistOrdered**.

**Fail-closed (no POST, no account commit, disk bytes unchanged where unsafe)**

| Store read result | Action |
|-------------------|--------|
| `SignedOut` | Allow OAuth if account still has refresh token |
| `InvalidJson` | Error to caller; no POST; no `auth.json` rewrite |
| `Unreadable` (mode/ephemeral/permission) | Error; no POST |
| `ensure_codex_home` / parent unsafe | Error on any write path; no create home/auth under unsafe parent |
| Keychain `Refused` | Propagate; no silent Proceed |

---

## Store read, Keychain cache, and invalidation

**`fresh` flag**

- `fresh: true` — bypass positive secret cache; still honor error cache TTL where specified.
- `fresh: false` — TTL reuse allowed only when stat proves entry unchanged.

**Keychain stat / cache (fix rr3 #1)**

| `keychain_stat` outcome | Non-fresh behavior |
|-------------------------|-------------------|
| **Missing** | Cache **error** (Missing) with `KEYCHAIN_TTL`; return error; **never** return previous cached secret |
| **Refused** | Cached error TTL; return error |
| **Unreadable** (stat/find) | Prefer 10m error cache (optional follow-up alignment); do not treat as unchanged secret |
| **Ok, mdat unchanged** | Return cached bytes; touch `checked` |
| **Ok, mdat changed** | Live `find`; refresh cache |
| **Stat fails (transient)** | May reuse last known secret **only if** last successful read exists and error is not Missing |

**Deletion:** Removing Keychain entry must invalidate cached bytes within one non-fresh read (test: two `StoreReader`s, delete between reads).

**File store:** `read_auth_file` already maps missing/invalid; reconciliation must not downgrade InvalidJson to Proceed.

---

## Persistence order and partial-failure recovery

**No claim:** OAuth success + Keychain write + `auth.json` + account commit cannot be one atomic transaction.

**Ordered steps after successful OAuth (200 + parseable token set)**

1. Build `next` SecretObject in memory (access, refresh if rotated, id_token, preserve `codex_home`, `account_id`, `last_refresh`).
2. **`lease.commit(None, next)`** — authoritative for hofund; must succeed before caller returns fresh access for upstream.
3. **Codex store write-back** under reconciliation lock: compare on-disk access to **pre-OAuth stale**; if still stale, `save_login` / Keychain put; if another writer changed access, skip overwrite (keep rr2 behavior).

**Failure matrix**

| Failure point | Durable state | User-visible / recovery |
|---------------|---------------|-------------------------|
| OAuth fails (4xx auth) | Account unchanged or `lapsed` flag on auth error (existing) | Sign in again |
| OAuth succeeds, **commit fails** | New tokens only in memory — **lost if process dies** | Retry refresh; account still stale; store may still have old refresh — **no secret loss** if old refresh valid |
| Commit succeeds, **Keychain/file write fails** | Account has new tokens; Codex CLI store stale | Next `access` uses account secret; adopt path can push to store on agent write; refresh may POST again only if store stale matches **old** stale — reconciliation must re-read store under lock before POST |
| Crash after commit, before write-back | Same as row above | Recoverable from account lease |
| Crash after OAuth, before commit | OAuth may have rotated refresh at issuer | Account+store still old pair; next POST uses old refresh — if issuer invalidated old refresh, user must sign in (existing OAuth risk) |
| Lock timeout + stale store | No POST if invalid store; if SignedOut POST allowed under lease only | Documented in timeout path |
| Adopt commit fails | Store newer than account | Retry; no POST |

**No plaintext rollback journal.** Recovery uses only account secret, Codex files, and Keychain.

---

## macOS concurrency notes

- Directory `flock` does not block same-process second acquirer — **in-process mutex required** (rr3 #5).
- Lock file recreate while held must still block second cross-process writer (existing test `r47b_rr3_lock_recreate_while_held_blocks_second_writer`).
- **`write_atomic_private` / `save_login`** must call the same **`ensure_codex_home` / unsafe-parent check** as lock acquire; no `create_dir_all` bypass (rr3 #6).

---

## Regression matrix (must hit production entrypoints)

| # | Scenario | Entry | Pass criteria |
|---|----------|-------|---------------|
| R1 | Store already rotated | `rotate_for_test` / `access` | 0 token POST; returned access = store |
| R2 | Second POST while OAuth in flight | `rotate_for_test` + concurrent lock | Second taker Busy or 0 POST after re-read |
| R3 | Lock timeout, store changed | `acquire` none path | 0 POST; no overwrite |
| R4 | Adopt | store ≠ lease | 0 POST; lease secrets = store (access+refresh+account_id) |
| R5 | Invalid `auth.json` | `rotate_for_test` | 0 POST; bytes unchanged |
| R6 | Keychain entry deleted | two `StoreReader`, non-fresh | error/Missing; no stale secret |
| R7 | Unsafe parent 0777 | `persist_refresh` / reconciliation write | no home/auth created |
| R8 | 401 Replace | `Need::Replace` | uses adopted lease token; refresh works next |
| R9 | Discover/import | `discover::list`, import | read paths respect cache + fail-closed |
| R10 | Detached refresh completes on cancel | `access` + drop waiter | commit + write-back if OAuth done |
| R11 | Partial crash simulation | test hooks | account vs store divergence recoverable per matrix |
| R12 | Error redaction | auth errors | no refresh/access in logs |
| R13 | FakeKeychain isolation | parallel tests | distinct homes; no cross-test cache bleed |

Extend existing `r47b_*` tests where possible; add rows for Adopt+commit, full-lock OAuth, Missing cache, unsafe home on write path.

---

## Security review criteria and gates

Independent of implementation PR:

1. **Author ≠ adversarial reviewer** — implementer cannot be sole security approver.
2. **Supervisor reviews full diff** against this spec and rr3 defect list (all six must be explicitly closed or waived with rationale).
3. **No merge** until: written spec approval (this document) **and** security signoff **and** unchanged full gates including `ALL_KILLED` (timeout failures treated as failures) **and** green CI **and** main merge policy (R-8a hold unchanged).
4. Tests must prove fail-closed paths and fake Keychain isolation (R12–R13).
5. **No tag/release** for R-47b.

---

## Spec self-review (inline)

- **Placeholders:** None; state names and orders are binding for implementation planning.
- **Contradictions:** Lock timeout allows POST only under lease + SignedOut or re-read stale match; aligns with fail-closed for InvalidJson/Unreadable.
- **Ambiguity:** “Unreadable” store does not authorize OAuth; only SignedOut or valid JSON with matching stale access does.
- **Scope:** Single plugin module refactor + tests; no daemon OAuth move.

---

## ─── Written Spec Approval Required ───

**STOP.** Palmer authorized scoped redesign at 2026-11-05 17:07 ET subject to **review and approval of this written spec**. Do **not** start patch #4, implementation plans, or code changes until explicit approval is recorded (e.g. supervisor comment or amended spec commit).

**Next step after approval:** implementation plan via `writing-plans` skill — **not part of this document**.
