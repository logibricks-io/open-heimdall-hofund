# R-47b: Codex credential reconciliation (design spec)

**Status:** **Written-review draft** — **explicit Palmer written spec approval required** before any implementation plan (supervisor comment alone is not approval). **STOP** here until then.

**Branch:** `sup/r-47b-codex-store`; rr3: `/workspace/hofund-review/r-47b-grok-rr3.md`.

---

## Review assumptions (nothing pre-approved)

1. Codex plugin only; production entrypoints: `tokens::access` / `rotate`, `discover`, `agent_sign_in::write`, `rotate_for_test` (production lock/order).
2. Durable stores: **protected account secret** (daemon via `lease`/`commit`, existing plugin-host contract), Codex `auth.json`, Keychain, `config.toml` — no new files/APIs/plaintext journals.
3. **Requires explicit Palmer approval** for: (a) in-blob protocol fields (`RefreshInFlight`, `CandidatePending`, fingerprints), (b) **home-owner** enforcement via **Codex-plugin-only** `list()` + each row’s secret metadata scan — **no other plugins, no new permissions**, (c) conservative **`RefreshInFlight` ⇒ reauth** when rotation ambiguous. Approval is **not** granted by this draft.
4. OAuth + Keychain + account **not atomic**; **no zero-loss**; external pre-write recheck **not atomic** (TOCTOU); fail-closed on mismatch.
5. R-8a / restriction holds unchanged.

---

## Scope / alternatives

Single reconciliation state machine: home owner, in-blob flight/pending, adopt vs OAuth, store write-back. No new product features or releases.

| Option | Verdict |
|--------|---------|
| **A.** One transaction + in-blob flight/pending | **Recommend** |
| **B.** Patch #4 | Reject (rr3) |
| **C.** Daemon OAuth | Reject (scope) |

---

## Canonical home & Keychain contract

- **Missing home:** validate ancestors + unsafe-parent rules; create **new** home **0700** only; **do not** `chmod` an **existing** safe home. Re-resolve to canonical absolute path (symlink target).
- **Locking key:** canonical home coordinates **local mutex + flock** and home-owner metadata — **must not** change existing Keychain **service** string or `keychain_account(home)` derivation; reads/writes use **existing** lookup contract only (no new items/queries for migration).
- **Alias regression:** two paths resolving to same canonical home → same lock slot + same owner binding; **no credential migration** between aliases.
- Inode/path change under composite lock → abort store write (non-CAS external TOCTOU acknowledged).

---

## UNIQUE home owner (composite-guard atomicity)

**Invariant:** One canonical home → at most one Codex `AccountId` may refresh/OAuth/bind.

**Protocol join (all binding writers):** `upsert`, agent sign-in, reconciliation OAuth/adopt — each must run home-owner check **under the same home composite guard** (lease → local mutex → flock) after **caller’s account lease** is taken. **Do not** acquire a **second** account lease while holding the home composite guard.

**Check:** Codex-scoped `list()` + secret `codex_home` canonical compare. **Publication:** bind/reject home in secret only while composite guard held so two concurrent “0 owner” upserts cannot both succeed.

| Owners | Action |
|--------|--------|
| 0 | Bind allowed only for caller under guard (agent rules) |
| 1 | Must be lease `AccountId` |
| ≥2 | Fail-closed refresh/OAuth for that home |

**Escalation:** If existing host APIs cannot enforce guard+publication without new permissions/interfaces → **STOP implementation**, escalate to Palmer (design blocker). **M1** below is **required future test design**, not evidence this doc executed tests.

---

## Lock, timeout, cancellation vs crash

**Order:** lease → local mutex → flock → cache mutex (brief cache ops only).

**Full composite required for:** pending restore/clear, fresh store read, OAuth POST, store write, adopt commit, home-owner scan/bind.

**Timeout Busy:** zero POST, write, commit, adopt.

**Cancellation:** After OAuth/request start, **detached refresh transaction** continues durability steps (InFlight → POST → CandidatePending → store write → clear attempts) **before** releasing composite guard; caller may cancel waiting **without** stopping an already-started durability job mid-protocol.

**Crash / kill:** surviving `RefreshInFlight` without proof POST unsent → **reauth**; **no** old-refresh retry. Distinct from cooperative cancel of waiter only.

**Logging:** **No** access, refresh, id_token, or fingerprint material in logs/errors (redaction regression).

**POST retry:** No automatic refresh POST retries unless provably unsent; conservative InFlight handling.

---

## Store states: SignedOut, Missing, Noop

Under **full guard + unique owner + safe home + account refresh present**:

| Live store read | Meaning | OAuth? |
|-----------------|---------|--------|
| **SignedOut / absent selected store** | Valid **ABSENT** baseline | Yes **after** InFlight commit; baseline fp = **ABSENT** |
| **Was present at baseline, now absent** | Deleted conflict | **No** POST; fail-closed |
| **InvalidJson / Unreadable** | Never → ABSENT | **No** POST; bytes unchanged |
| **Keychain Refused** | Fail-closed | **No** POST |
| **Tuple matches account; `Need` satisfied** | **Noop** | **No** POST (`IfExpiring` fresh, `Replace` mismatch) |
| **Tuple matches; only `Need` requires rotate** | Stale per Need | OAuth path |

**CandidatePending** may **create** missing store **only** if live baseline fp was **ABSENT** and step 4 baseline still ABSENT/match. Baseline had credentials → disappearance is conflict, not auto-seed.

---

## Fingerprints & protocol (ordered)

Fp = store location + **full tuple** (access, refresh, id_token, store `account_id` hashes). Same access, different refresh → different fp → adopt/readopt + **401 `Replace`** uses **current** lease access after wait.

| Step | Action | Fail |
|------|--------|------|
| 0 | Pending restore/clear (locked, live) | conflict → stop |
| 1 | `commit RefreshInFlight` | **0 POST** |
| 2 | Single POST | ambiguous → InFlight + reauth |
| 3 | `commit CandidatePending` + tuple | keep InFlight; reauth if candidate lost |
| 4 | Conditional store write (baseline match) | repair later |
| 5 | Clear markers commit | retry clear; failed clear **regression** |

Restart: InFlight ambiguous → reauth; CandidatePending → repair **0 POST** then clear.

---

## Adopt & expired store access

Usable store access → `commit` full tuple → success **0 POST**. Expired access → `commit` full tuple → **retryable refresh-needed** this call (**no** POST); next `access`/`rotate` POST with **adopted refresh** under full guard. Full-tuple readopt on 401 after adopt.

---

## Keychain cache (discover / non-reconcile)

Post-TTL validation: **Missing** → error, no stale secret; **fresh** reconcile bypasses pos/neg cache. **Refused/Unreadable** on reconcile → 0 POST. Same home, two `FakeKeychain` providers → isolated cache (regression on `StoreReader` + entrypoints).

---

## State flow

```text
lease → composite (or Busy)
  → owner scan/bind under guard
  → pending restore (locked)
  → fresh read → refuse/invalid fail
  → noop if Need satisfied
  → adopt paths (0 POST rules)
  → else InFlight → POST → Pending → write → clear
```

---

## Fail-closed matrix

| Condition | POST | write | commit* |
|-----------|------|-------|---------|
| InvalidJson / Unreadable / Refused | No | No | No |
| Deleted-after-baseline | No | No | No |
| Lock Busy | No | No | No |
| Owner conflict | No | No | No |
| Pending conflict | No | No | No |

\*Protocol steps 1/3/5 when prior step succeeded.

---

## Regression matrix (production entrypoints; not yet executed)

| ID | Must prove | Entry |
|----|------------|-------|
| M1 | Two accounts, same home → fail before POST | `rotate_for_test` |
| M2 | InFlight before POST; step-1 fail → 0 POST | `rotate_for_test` + hooks |
| M3 | Ambiguous OAuth → reauth | `access` detached |
| M4 | Candidate commit fail; clear fail; crash each step | `rotate_for_test` / `access` |
| M5 | Pending repair; no rollback | `access` |
| M6 | Expired adopt then next refresh uses new refresh | `access` |
| M7 | 401 Replace + full tuple readopt | `access` |
| M8 | Lock timeout zero side effects | `rotate_for_test` |
| M9 | Double POST prevented | `rotate_for_test` |
| M10 | Invalid/unreadable/refused 0 POST | `rotate_for_test` |
| M11 | Unsafe home; preserve existing perms | agent + write-back |
| M12 | macOS Busy; alias same lock | lock + M1 path |
| M13 | Cache Missing post-TTL; dual FakeKeychain | `StoreReader` + discover |
| M14 | Error redaction | all entrypoints |
| M15 | rr3 #1–#6 | map above |

---

## After Palmer approves spec (not executed for this doc)

1. **`writing-plans`** → implementation.  
2. **Fresh implementation context** (do not reuse author design chat for code review).  
3. **Read-only adversarial reviewer ≠ author**; supervisor **full-diff** review.  
4. Merge only with **GATES_FAILED=0**, **ALL_KILLED** (timeouts **not** killed), green CI, merge **`origin/main`** without rebase/force-push, R-8a dependency satisfied, security signoff, no sensitive-feature bypass, **no release**.  
5. This document **does not** claim any gate or test has been run.

---

## Self-review

- §3: explicit Palmer required; no “assumed approval.”  
- SignedOut **ABSENT** vs deleted; Noop; Refused fail-closed.  
- Owner check under composite guard; no second lease; binding writers listed; M1 = future proof only.  
- Keychain derivation unchanged; existing home perms preserved.  
- Cancel vs crash; redaction; entrypoint tests specified.

---

## ─── Written Spec Approval Required ───

**STOP.** Palmer **explicit written spec approval** → then `writing-plans` only.
