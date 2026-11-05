# R-47b: Codex credential reconciliation transaction (design spec)

**Status:** Draft — **Palmer written spec approval required** before any implementation (supervisor review alone is not approval).

**Revision:** Addresses supervisor self-review of `5622276` (lock timeout, pending sync, cache Missing, adopt tuple, matrix, approval gates).

**Branch:** `sup/r-47b-codex-store`; rr3 defects in `/workspace/hofund-review/r-47b-grok-rr3.md`.

---

## Review assumptions (explicit)

1. **Scope:** Codex subscription plugin only (`plugins/subscriptions/codex`); same production entrypoints (`tokens::access` / `rotate`, `discover`, `agent_sign_in::write`, `rotate_for_test` mirroring production ordering).
2. **Durable stores:** account lease secret, Codex `auth.json`, Keychain (home-derived account id), `config.toml` — **no new files, APIs, or plaintext journals**.
3. **Pending sync metadata** may live **inside the existing encrypted account secret blob** (non-secret fields: canonical home path, store-location fingerprint, pre-write baseline access fingerprint, candidate tuple version marker). **Await Palmer written approval** for this encoding; it is not a new store.
4. OAuth, Keychain, and account commit are **not one atomic transaction**; ambiguous OAuth success (response lost, crash before first durable candidate record) may require **re-sign-in** — **do not** automatically retry the pre-OAuth refresh token.
5. R-8a merge hold, restriction holds, `ALL_KILLED`, timeout-as-failure CI — unchanged. rr3 optional follow-ups (import account_id, chmod, argv) remain out of scope.
6. hofund does **not** exclude non-cooperating external writers of `codex_home`; design assumes recheck-under-lock and fail-closed on conflict.

---

## Bounded scope and non-goals

**In scope:** One reconciliation state machine per `(AccountId, canonical codex_home)` owning compare, pending repair, OAuth vs adopt, ordered durable steps, composite lock lifetime.

**Non-goals:** New sign-in flows, features, secondary DBs, claiming cross-process exclusion against arbitrary third parties, tag/release/merge without gates below.

**Recommendation (unchanged):** Single transaction (option A) vs patch #4 vs daemon OAuth — **A only**; see prior table in git history if needed.

---

## Canonical identity and path safety

- **Canonical codex_home:** `canonicalize` (or equivalent) at reconciliation boundary; lock key, Keychain account id, and metadata use **canonical path**. Symlink/alias pairs that resolve to the same directory share one lock slot; divergent resolution after re-read → **fail-closed conflict**.
- **Inode / home replacement:** After composite lock, record `(dev, ino)` (or platform equivalent) of `codex_home` directory; before any store write, re-stat; change while lock held → **abort write**, fail-closed (possible external rename/replace).
- **SignedOut / create paths:** `ensure_codex_home` + unsafe-parent check run **before** any path treats store as absent or creates home/auth (read, OAuth eligibility, write-back, agent write).

---

## Lock hierarchy, acquisition order, lifetime

**Overall acquisition order (never invert):**

1. **Account lease** (`LEASE_WAIT` 30s) — held for entire transaction including all commits and OAuth.
2. **Local mutex** — keyed by canonical `codex_home` (same-process serialization; macOS `flock` reentrancy fix).
3. **Cross-process flock** — directory lock on `codex_home` inode (survives `.codex.lock` file recreate).
4. **Keychain cache mutex** — only while updating/reading cache structs; **never** across OAuth HTTP.

**Composite rule:** OAuth POST and any Codex store **write** require **lease + local mutex + flock** all held. Keychain cache reads during reconciliation use **fresh/live** path (below); no lock held on cache during POST.

**Lifetime:** Hold composite lock from first authoritative store read through final store write or explicit skip, including all `lease.commit` steps that are part of the same transaction (candidate + clear-marker commits).

**Composite lock timeout (bounded, retryable, fail-closed):**

- Retry local→flock acquisition until deadline (10s / 50ms unchanged).
- On **Busy** or **Io** at deadline: **release any partial composite guards**; return retryable error to caller (`Busy`-class). **No OAuth. No store write.**
- Under **lease only**, caller may **`fresh` re-read** store (bypass Keychain positive/negative cache) and, if store presents a **full usable credential tuple** strictly newer/different from account lease with **valid non-expired access** (or within policy below), perform **Adopt** via `lease.commit` **without** composite lock — **0 POST**. This is the **only** lock-timeout side effect; it does **not** serialize with other account leases on the same home (hence no OAuth without lock).
- **Never:** POST on timeout, even if store is SignedOut; “same decisions as locked path” is **removed**.

**External writers:** No guarantee other processes respect flock; reconciliation **re-reads store under full composite lock** immediately before conditional write; baseline fingerprint mismatch → pending repair or conflict, not blind overwrite.

---

## Reconciliation store reads (authoritative path)

Under **full composite lock**, every reconciliation **store read is fresh:**

- Bypass Keychain **positive** secret cache and **negative** error cache for that read path.
- **No** cached or transient Keychain fallback may authorize OAuth or adoption on this path.
- Discover/import may keep TTL reads; **refresh/adopt/OAuth/write-back** use the fresh locked read only.

---

## Keychain cache semantics (non-reconciliation reads)

For ordinary `read_login(..., fresh: false)` (discover, etc.):

| Event | Rule |
|-------|------|
| **`Missing` observed** (stat or find) | Record Missing in cache; **zero** reuse of prior cached bytes **forever** until a successful live read replaces cache entry |
| **After success TTL expires** | Next read that **requires validation** (mdat check) runs stat; if **Missing** → error, no stale secret (deletion detected at validation boundary, not “within one non-fresh read” colloquially) |
| **`Refused` / `Unreadable`** | Classified errors; cache with existing TTL constants; **not** treated as transient stat success |
| **Transient stat failure** (I/O flake, not Missing/Refused/Unreadable) | May reuse last **successful** bytes **only** on non-reconciliation reads, and never after Missing was recorded |

**Reconciliation:** always fresh under lock — table above does not authorize POST/adopt on stale cache.

**Tests:** Same canonical home, **two distinct `FakeKeychain` provider instances** — cache must not cross providers (not merely distinct home paths).

**No new 10m Unreadable TTL policy** in this redesign; negative TTL must not block pending repair under lease.

---

## Pending store sync (eliminate adopt rollback)

**Problem:** Account-first commit then failed store write lets a later transaction **adopt stale store** and effectively roll back new credentials.

**Protocol (existing stores only):**

After OAuth yields an accepted in-memory **candidate tuple** (access, refresh, id_token, account_id, exp-derived freshness):

1. **`lease.commit`** candidate plus **pending marker** in account blob: canonical `codex_home`, selected store mode/location fingerprint, **baseline fingerprint** (access hash/id at lock-time read), marker id/version.
2. Under **full composite lock**, re-read store; if baseline still matches (same stale access as at OAuth start) → conditional **store write** of candidate; on success → **`lease.commit`** same secrets **without** pending marker (clear).
3. If store access already changed by cooperating writer → skip overwrite; clear pending only if store tuple matches candidate; else **conflict** (below).

**Pending recovery (runs before generic Adopt or OAuth on every refresh entry, under lease):**

| Observed store vs marker | Action |
|--------------------------|--------|
| Matches **candidate** tuple | Clear marker if store already written; else finish write under lock, then clear |
| Matches **baseline**, not candidate | **Repair:** write candidate to store under lock; clear marker — **0 POST** |
| **Third** tuple, missing when baseline expected, invalid/unreadable JSON, unsafe home | **Fail-closed conflict** — error; **no** adopt-back; **no** POST with old refresh |
| Marker present, OAuth never durably recorded | Treat as incomplete prior attempt; repair path only if candidate fields present in blob |

**Failure precision:**

| Point | Durable state | Next entry |
|-------|---------------|------------|
| OAuth OK, **first commit (candidate+pending) fails** | Issuer may have rotated; blob unchanged | Retry transaction; **no** blind old-refresh POST if ambiguous rotation |
| Pending commit OK, **store write fails** | Account has candidate+pending; store old | **Pending recovery** repair (0 POST) |
| Store OK, **clear-marker commit fails** | Account pending+candidate; store new | Retry clear; do not adopt baseline |
| Crash / cancel before step 1 | Old account+store | Normal reconcile; no pending |
| Crash after pending, before store | Pending+candidate in account | Repair path |
| OAuth response lost / crash before step 1 with suspected issuer rotation | Ambiguous | **Fail closed toward re-sign-in**; **no** automatic old-refresh retry |

---

## State machine (single entry)

```text
Start (lease) → PendingRecovery? → take composite lock (or timeout adopt-only path)
  → fresh ReadStore + home inode check
  → Invalid/Unreadable/unsafe → FAIL
  → usable store tuple ≠ lease → AdoptFull (0 POST) if access fresh enough
  → else stale match + pending clear path / OAuth (lock required)
  → OAuth → candidate+pending commit → locked write → clear marker
  → release composite → Done
```

**OAuth:** Only with composite lock, SignedOut or valid JSON with access matching account stale, pending recovery clear, home valid.

**Adopt full tuple:** Commit access, refresh, id_token, account_id, `codex_home`, expiry-related fields needed for `fresh()` / `Need::*`; **0 POST** only if adopted access is **usable** (not known-expired per `REFRESH_MARGIN` policy — if store access expired, **must OAuth or fail**, never return success with dead access).

**Adopt `lease.commit` fails / cancelled:** No mutation; caller gets error; **Need::Replace** on retry compares **current** lease access (post-wait), not pre-adoption stale from an earlier read.

**Need semantics (after successful reconcile):**

- **`IfExpiring`:** POST path uses **account** refresh after adopt/repair; adopted pair then near expiry → POST uses **store/account refresh**, not pre-adopt refresh.
- **`Replace(stale)`:** Compare backend-stale to **issued tuple’s current access** at reconcile entry (after lease wait), not a cached pre-adopt value; if different candidate already in lease, 0 POST.

---

## Fail-closed summary

| Condition | OAuth | Store write | Adopt from store |
|-----------|-------|-------------|------------------|
| InvalidJson | No | No | No |
| Unreadable | No | No | No |
| Unsafe home / parent | No | No | No |
| Composite lock timeout | No | No | Only full fresh tuple, non-expired |
| Pending conflict / third tuple | No | No | No |
| Keychain Refused | No | No | No |

---

## Regression matrix (production entrypoints)

| # | Invariant | Entry |
|---|-----------|-------|
| M1 | Adopted pair; later expiry refresh POST uses **new** refresh | `access` / `rotate_for_test` |
| M2 | 401 `Need::Replace` after adopt compares **current** lease access | `access` |
| M3 | Missing at validation after TTL; fresh reconcile never serves deleted secret | `StoreReader` + reconcile |
| M4 | InvalidJson + Unreadable: 0 POST, bytes unchanged | `rotate_for_test` |
| M5 | Unsafe home: **all** write paths (incl. write-back, agent write) | reconciliation + `persist_refresh` |
| M6 | macOS same-process Busy; cross-process lock recreate; home alias/inode replace aborts write | lock tests + reconcile |
| M7 | Every durability failure: pending repair, no adopt-rollback | simulated fail/crash hooks |
| M8 | OAuth cancel/response-loss: no silent old-refresh retry | detached job tests |
| M9 | Lock timeout: **never POST**, even SignedOut; optional lease-only adopt if fresh tuple | `rotate_for_test` |
| M10 | Same home, two FakeKeychain providers — no cache cross-talk | parallel tests |
| M11 | Full-lock OAuth: second taker Busy, 0 double POST | `rotate_for_test` |
| M12 | Error redaction | auth errors |
| M13 | rr3 #1–#6 behaviors explicitly covered | map to tests above |

Prior R1–R13 rows superseded by M1–M13 where overlapping.

---

## Security review and approval gates

1. **Independent read-only adversarial reviewer** — **not the implementer**; review runs **after** implementation exists (fresh code context), against this spec + rr3 list.
2. **Supervisor full-diff review** — mandatory; **cannot waive** rr3 security defects by rationale alone; each must be **closed in code** with test evidence.
3. **Merge blocked** until: **Palmer explicit written spec approval** + adversarial security signoff + supervisor full diff + unchanged CI gates (`ALL_KILLED`, timeouts fail) + R-8a policy.
4. **Supervisor comment or spec commit alone ≠ Palmer approval.**

---

## Spec self-review (inline)

- **Contradictions removed:** Lock timeout no OAuth/SignedOut POST; diagram timeout branch deleted; reconciliation never uses cache fallback for POST/adopt.
- **Rollback:** Pending marker + repair-before-adopt/POST is concrete; “account authoritative” vague wording removed.
- **Missing cache:** Aligned to post-TTL validation + Missing poisons cache; fresh locked reads bypass negatives.
- **Placeholders:** None; pending field names are illustrative — exact JSON keys are implementation detail inside blob.
- **Scope:** No new stores; metadata-in-blob assumption flagged for Palmer.

---

## ─── Written Spec Approval Required ───

**STOP.** Implementation plan and patch #4 are **forbidden** until **Palmer’s explicit written spec approval** (not supervisor-only). Next step after Palmer approval: `writing-plans` — not in this document.
