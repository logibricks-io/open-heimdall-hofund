# R-47b: Codex credential reconciliation (design spec)

**Status:** **Approved for implementation** — Palmer explicit written-spec approval recorded 2026-11-05 ~21:40 ET (America/New_York). Approved items: (a) in-blob fields, (b) Codex-plugin-only home-owner scan via `list()` + secret metadata (no other plugins / no new permissions), (c) conservative RefreshInFlight ⇒ reauth when rotation is ambiguous. Independent security review required before merge. Proceed with `writing-plans` then fresh implementation context.

**Branch:** `sup/r-47b-codex-store`; rr3: `/workspace/hofund-review/r-47b-grok-rr3.md`.

---

## Review assumptions (nothing pre-approved)

1. Codex plugin only; entrypoints: `tokens::access` / `rotate`, `discover`, `agent_sign_in::write`, `rotate_for_test` (production lock/order).
2. Durable stores: **protected account secret** (daemon via `lease`/`commit`, existing plugin-host contract), Codex `auth.json`, Keychain, `config.toml` — no new files/APIs/plaintext journals.
3. **Requires explicit Palmer approval** for: (a) in-blob fields below, (b) home-owner via **Codex-plugin-only** `list()` + secret metadata scan — **no other plugins, no new permissions**, (c) conservative **`RefreshInFlight` ⇒ reauth** when rotation ambiguous. **Not** pre-approved.
4. OAuth + Keychain + account **not atomic**; **no zero-loss**; external recheck **not CAS** (TOCTOU).
5. R-8a / restriction holds unchanged; no new features/access beyond this spec.

---

## Scope / alternatives (tradeoffs)

Single state machine: home owner, markers, adopt vs OAuth, store write-back. No releases.

| Option | Tradeoff | Verdict |
|--------|----------|---------|
| **A** Single transaction + in-blob markers | Better crash recoverability; cost = internal blob states + protocol complexity | **Recommend** |
| **B** Local patch #4 | Smaller diff; rr3 proved cross-cutting invariants still fail | Reject |
| **C** Daemon-owned OAuth | Clear lock owner; boundary + scope expansion vs plugin-host | Reject |

---

## In-blob fields and marker precedence

Protocol metadata and candidate secrets stay inside the existing protected account-secret JSON; no plaintext journal. (Exact keys = implementation detail.)

| Field | When set |
|-------|----------|
| `refresh_attempt_id` | Step 1 RefreshInFlight |
| `refresh_canonical_home` | Step 1 |
| `refresh_store_location_fp` | Step 1 (file vs keychain + mode) |
| `refresh_baseline_tuple_fp` | Step 1; literal **ABSENT** when store signed out |
| `refresh_candidate_tuple_fp` | Step 3 CandidatePending |
| Candidate secrets | access, refresh, id_token, store account_id in blob at Step 3 |
| `refresh_invalid` | Unrecoverable conflict marker; **fail-closed** on refresh until operator clears via re-sign-in |

**Phases (mutually exclusive active marker):** at most one of InFlight-only, CandidatePending (includes candidate tuple + fps), or INVALID. If legacy blob has both InFlight and Pending, **CandidatePending wins**; repair uses Pending tuple; clear both when done.

**Fingerprints:** store location + hashed full tuple (access, refresh, id_token, store account_id). Never log secrets or fp values.

---

## Canonical home & Keychain contract

Missing home: validate ancestors + unsafe parent; create **new** home **0700** only; do not chmod existing safe home. Canonical absolute path (symlink target) keys mutex/flock/owner metadata. **Do not** change Keychain service or `keychain_account(home)` derivation. Alias paths → same canonical lock + owner. Inode/path change under composite lock → abort store write.

---

## UNIQUE home owner

One canonical home → at most one Codex `AccountId`. Binding writers (`upsert`, agent sign-in, reconcile) run owner scan + publication **under same home composite guard** after caller **account lease** only; **no second account lease** while home guard held. Codex `list()` + secret `codex_home` canonical compare.

| Owners | Action |
|--------|--------|
| 0 | Bind caller under guard only |
| 1 | Must equal lease AccountId |
| ≥2 | Fail-closed refresh/OAuth |

Cannot enforce with existing APIs → **STOP implementation**, escalate Palmer. **M1** = required future test, not executed by this doc.

---

## Lock, waits, flock, cancellation

**Order:** lease (wait **30s**) → local mutex → directory **flock** on codex_home inode → cache mutex (brief).

**Waits:** home composite **10s** total, **50ms** retry; on Busy/Io → release partial guards → retryable Busy with **zero** POST/write/commit/adopt.

**Lifetime:** full guard spans fresh baseline read → OAuth POST → **both** durable account commits **and** store write attempts before release.

**Flock:** directory lock stable across `.codex.lock` delete/recreate; second cross-process taker **Busy** while inode lock held. macOS + Linux same-process mutex required (second taker Busy).

**Cancellation:** waiter may drop; **detached** refresh job continues durability (InFlight → POST → Pending → store → clear) before guard release — cancel is wait-safe, not protocol-abort mid-flight.

**Crash/kill:** InFlight without proof POST unsent → reauth; no old-refresh retry.

**Reconcile reads:** **fresh** under full guard; bypass Keychain **positive and negative** cache; no cache fallback for POST/adopt.

---

## Store states (baseline fresh under full guard)

| Live read | OAuth in same call? |
|-----------|---------------------|
| SignedOut / absent store (ABSENT baseline fp) | Yes after InFlight commit |
| Was credentialed at baseline, now absent | No; deleted conflict |
| InvalidJson / Unreadable / Keychain Refused | No; bytes unchanged |
| Tuple matches account; Need satisfied | **Noop**; no POST |
| Need requires rotate; tuple stale vs Need | OAuth after markers |

Pending may create store only if baseline fp was **ABSENT** and step 4 still ABSENT/match.

---

## OAuth protocol (ordered)

Prerequisite: lease + full guard + fresh baseline + owner OK. **Noop/adopt must not bypass** CandidatePending/INVALID (step 0 first).

| Step | Durable / network | On failure |
|------|-------------------|------------|
| 0 | **CandidatePending repair** (table below) | conflict → stop; **no generic OAuth in same call after repair branch** |
| 1 | commit RefreshInFlight + fps above | **0 POST** |
| 2 | Single OAuth POST (guard held) | only **definitive non-rotating issuer failure** may clear InFlight; unknown HTTP/timeout response **keeps** marker → reauth; transport **must not** auto-retry refresh POST unless provably unsent |
| 3 | commit CandidatePending + candidate tuple | **Keep InFlight**; no claim recoverable candidate if commit fails; reauth if tokens lost in memory |
| 4 | Re-read store; if baseline fp match → conditional write candidate | if third tuple vs baseline → **skip write**, **KEEP Pending**, **RETURN conflict**; **DO NOT run step 5** |
| 5 | commit clear markers + full tuple secrets | failed clear **retains Pending**; store may hold candidate; next call verify + clear |

Restart: ambiguous InFlight → reauth; Pending → step 0 repair only (**0 POST**).

### CandidatePending repair (step 0; full guard + fresh read)

| Live store vs pending | Action | Same-call OAuth after? |
|-----------------------|--------|------------------------|
| Matches **candidate** fp | After verified durable store (if write needed), clear markers; return candidate **only if** access still usable; if expired → **retryable refresh-needed** after clear (**no** known-expired success); same call **no POST**; next call new refresh under full guard | No |
| Matches **baseline** fp or explicit **ABSENT** | Conditional write candidate; after verified durable store, clear; same usable vs expired return rules as candidate-match row | No |
| Third tuple / unexpected missing / invalid / unreadable / refused / unsafe | **Conflict**; **keep Pending unchanged** (no commit); no adopt-back | No |

Failed step 5: Pending + candidate remain; next entry repeats step 0 before noop/adopt/OAuth.

---

## Adopt & expired access

Usable access → commit full tuple → success **0 POST**. Expired → commit full tuple → **retryable refresh-needed** (**no** POST); next call OAuth with adopted refresh under full guard. **Need::Replace** compares **current** lease access. Full tuple readopt on 401.

---

## Keychain cache (non-reconcile)

**Missing** observed: never return prior **successful** bytes again; negative cache TTL expiry permits **live find** retry. After TTL, validation that observes Missing → error. **Fresh** (`fresh: true`) bypasses positive **and** negative caches. Reconcile always fresh under guard. Same canonical home, two `FakeKeychain` providers → isolated cache (M13).

---

## Fail-closed matrix

| Condition | POST | write | commit* |
|-----------|------|-------|---------|
| Invalid / unreadable / refused | No | No | No |
| Deleted-after-baseline | No | No | No |
| Lock Busy | No | No | No |
| Owner / pending / INVALID conflict | No | No | No |

\*Steps 1/3/5 when prior step succeeded.

---

## Regression matrix (not executed)

| ID | Prove | Entry |
|----|-------|-------|
| M1 | Two accounts same home fail before POST | `rotate_for_test` |
| M2–M4 | Protocol steps + clear fail + crash hooks | `rotate_for_test`, `access` |
| M5 | Pending repair; no rollback; delayed Pending candidate expiry after repair clear | `access` |
| M6–M7 | Expired adopt; 401 Replace + readopt | `access` |
| M8 | Lock timeout zero side effects | `rotate_for_test` |
| M9 | No double POST | `rotate_for_test` |
| M10 | Invalid/unreadable/refused 0 POST | `rotate_for_test` |
| M11 | Unsafe home; preserve perms | agent + write-back |
| M12 | macOS **same-process Busy**; **cross-process** lock-file recreate Busy; home inode change abort; alias same coordination | lock tests + reconcile |
| M13 | Missing post-TTL; dual FakeKeychain same home | `StoreReader`, discover |
| M14 | Error redaction | all entrypoints |
| M15 | rr3 #1–#6 | map above |

---

## After Palmer approves spec (not executed)

1. `writing-plans` then **fresh implementation context** (no author design chat for review).
2. Read-only adversarial reviewer **≠ author**; supervisor full-diff.
3. Merge only: **GATES_FAILED=0**, **ALL_KILLED** (timeouts not killed), green CI, merge **origin/main** without rebase/force-push, R-8a + security signoff, no release.

---

## Self-review

Final five mechanical fixes in this revision (doc text only; **not** runtime proof): (1) in-blob intro wording, (2) step 4 third-tuple skip/keep Pending/no step 5, (3) repair success rows usable vs expired-after-clear, (4) conflict row keeps Pending without INVALID commit, (5) step 2 definitive-fail vs ambiguous HTTP + no auto POST retry.

---

## ─── Written Spec Approval Required ───

**Approved.** Palmer explicit written-spec approval received 2026-11-05 ~21:40 ET. Proceed with the post-approval sequence in the previous section.
