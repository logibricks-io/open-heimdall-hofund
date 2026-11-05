# R-47b: Codex credential reconciliation (design spec)

**Status:** Draft — **Palmer written spec approval required** (supervisor comment alone is not approval). **STOP** before implementation plan.

**Branch:** `sup/r-47b-codex-store`; rr3 defects: `/workspace/hofund-review/r-47b-grok-rr3.md`.

---

## Review assumptions

1. Codex plugin only; entrypoints: `tokens::access` / `rotate`, `discover`, `agent_sign_in::write`, `rotate_for_test` (same ordering as production).
2. Durable stores: **protected account secret** (daemon store via `lease`/`commit`, per existing plugin-host contract), Codex `auth.json`, Keychain, `config.toml` — no new files/APIs/plaintext journals.
3. **In-blob protocol fields** (`RefreshInFlight`, `CandidatePending`, fingerprints) inside existing account secret JSON — Palmer approval assumed for encoding only; not a new store.
4. OAuth + Keychain + account are **not atomic**; **no zero-loss guarantee**; tests inject failure at each phase.
5. R-8a hold, restriction holds unchanged. External writers: **flock does not block non-cooperating processes**; recheck-under-lock is **not CAS** — TOCTOU remains; fail-closed on mismatch.

---

## Scope / non-goals

One reconciliation state machine per refresh path: compare, home-owner check, in-blob flight/pending protocol, OAuth vs adopt, store write-back. No new sign-in features, tags, releases, or merge without gates below.

### Alternatives (inline)

| Option | Summary | Verdict |
|--------|---------|---------|
| **A. Single transaction + in-blob flight/pending** | Composite lock spans live read → durable markers → single POST → commits → store write | **Recommend** |
| **B. Patch #4 on split functions** | rr3 showed repeated gaps | Reject |
| **C. Daemon-owned OAuth** | New boundary | Reject (scope) |

---

## Canonical home (missing path OK)

1. Resolve user path; **if missing**, validate ancestor chain + unsafe-parent rules (`ensure_codex_home` semantics), create `0700` home privately, then re-resolve.
2. **Canonical key** = stable absolute path after resolution (symlink target); lock + Keychain account id + metadata use this key.
3. Re-resolve before lock and before write; path/inode change under composite lock → **abort write**, fail-closed conflict.

---

## UNIQUE home owner (same plugin)

**Risk:** Two `AccountId` rows with same canonical `codex_home` → account B could POST old refresh while A holds pending repair.

**Invariant:** At most **one** Codex-plugin account may bind a canonical home for refresh/OAuth/adopt.

**Enforcement (existing APIs only):** Before OAuth or pending restore, holder has **lease** on caller `AccountId`; scan **`host.accounts().list()`** (codex-scoped) + read each row’s secret for `codex_home`; canonicalize and count matches.

| Count | Action |
|-------|--------|
| 0 | OK only if caller blob will set home on this path (agent write rules) |
| 1 | Must equal lease `AccountId`; else fail-closed |
| ≥2 | **Fail-closed** on all refresh/OAuth for that home until operator fixes bindings |

**On `upsert` / agent sign-in:** reject binding canonical home already owned by another account id (including id relabel — no second lease identity for same home). **No reads of other plugins’ accounts.**

**Design blocker:** None if scan+reject is implemented; **proof:** test two accounts same home → second `rotate_for_test` fails before POST.

---

## Lock order and timeout

**Acquire order:** lease → local mutex (canonical home) → directory flock → cache mutex (cache updates only).

**OAuth, store write, pending restore/clear, adopt commit:** require **full composite** held.

**Reconciliation reads:** fresh/live under composite lock; bypass Keychain positive **and** negative caches; no cache fallback for POST/adopt.

**Timeout (10s / 50ms):** on Busy/Io → release partial guards → return **retryable Busy**. **Strict no side effects:** no POST, no store write, **no** `lease.commit`, **no** adopt.

**POST transport:** HTTP stack must not silently retry refresh POST unless implementation proves request **never reached issuer** (then clear `RefreshInFlight` only). **Automatic refresh POST retries disabled** when `RefreshInFlight` or ambiguous rotation could result.

---

## Tuple fingerprint (never log secrets)

Fingerprint covers: **store location** (file vs keychain + mode fingerprint), **logical full tuple** — hashes of access, refresh, id_token, store `account_id` (access alone insufficient). Same access + different refresh → **different** fingerprint → full-tuple adopt/OAuth decision.

---

## Account-blob protocol (ordered)

Under **lease + full composite + live fresh read + baseline validation + home-owner OK**:

| Step | Durable action | On failure |
|------|----------------|------------|
| 0 | If `RefreshInFlight` or `CandidatePending` → **pending restore** (below) | — |
| 1 | `commit RefreshInFlight{attempt_id, canonical_home, store_fp, baseline_fp}` | **Zero POST**; return error |
| 2 | **Single** OAuth POST (lock still held) | Clear InFlight only on definitive non-rotating auth error; else keep InFlight → **reauth** if ambiguous |
| 3 | Validated response → `commit CandidatePending + candidate tuple` | **Retain InFlight**; candidate not recoverable from blob; optional best-effort store write under lock **not guaranteed**; caller **reauth** if tokens lost |
| 4 | Re-read store; baseline match → conditional store write | Repair later if write fails |
| 5 | `commit` clear InFlight + Pending (full tuple, no markers) | Retry clear; store may be ahead of marker |

**After restart:** `RefreshInFlight` without proven unsent POST → **reauth-required**, **no** automatic old-refresh retry. `CandidatePending` → repair store under full lock (0 POST) then clear.

**Pending restore/clear:** only under **full composite + live verify**; diagram order fixed below.

| Store vs pending | Action |
|------------------|--------|
| Matches candidate fp | Finish write if needed; clear markers |
| Matches baseline fp, not candidate | Write candidate; clear — **0 POST** |
| Third fp / invalid / unreadable / unsafe | Fail-closed conflict; no adopt-back; no POST |

Generic adopt/OAuth runs **only after** pending branch completes or finds no markers.

---

## Adopt (0 POST) and expired access

Compare **full tuple** fp to lease; if store presents usable access (`fresh()` true) → `commit` full adopted tuple → return success, **0 POST**.

If store tuple valid but access **known expired** (inside `REFRESH_MARGIN`):

1. `commit` full store tuple to lease (refresh + id + account_id included).
2. **This call:** return **retryable refresh-needed** error — **no POST**, no expired access as success.
3. **Next call:** lease matches store fp; `IfExpiring` path may OAuth **once** under full guard using **adopted refresh**.

Same access / different refresh on store → treat as tuple change → adopt commit (0 POST) or conflict per fp rules, never silent ignore.

**Adopt commit fails:** no success return; lease unchanged.

**Need::Replace:** compare to **current** lease access after lease acquired, not pre-adopt stale.

---

## Keychain cache (non-reconcile reads)

`Missing` poisons cache permanently until live success. Post-TTL validation: stat Missing → error, no stale bytes. `Refused`/`Unreadable` ≠ transient stat. Reconcile ignores negative TTL blocking repair.

Tests: **same canonical home**, two `FakeKeychain` instances — no shared cache entry.

---

## State diagram

```text
lease → composite lock (or Busy exit)
  → home-owner scan
  → pending restore/clear (live, locked)
  → fresh ReadStore + inode check
  → invalid/unreadable/unsafe → FAIL
  → tuple fp ≠ lease: usable → Adopt; expired → commit + refresh-needed error
  → tuple fp = lease stale: InFlight commit → POST → CandidatePending → store write → clear
  → release composite
```

---

## Fail-closed matrix

| State / error | POST | commit | store write |
|---------------|------|--------|-------------|
| InvalidJson / Unreadable | No | No* | No |
| Unsafe home | No | No* | No |
| Lock timeout Busy | No | No | No |
| Home owner conflict | No | No | No |
| Pending conflict | No | No | No |
| RefreshInFlight ambiguous | No | No** | No |

\*Except steps 1/3/5 of protocol when prior steps succeeded. \*\*Reauth path only.

---

## Regression matrix (M1–M14)

| ID | Invariant | Entry |
|----|-----------|-------|
| M1 | Two accounts same home → fail before POST | `rotate_for_test` |
| M2 | InFlight before POST; fail commit → 0 POST | injected |
| M3 | Ambiguous response → InFlight → reauth, no old refresh | injected |
| M4 | Candidate commit fail → InFlight; no fake recoverable candidate | injected |
| M5 | Pending repair 0 POST; no adopt rollback | reconcile |
| M6 | Usable adopt 0 POST; expired adopt → error then next POST new refresh | `access` |
| M7 | Replace uses current lease access | `access` |
| M8 | Lock timeout: zero side effects | `rotate_for_test` |
| M9 | Full-lock OAuth: no double POST | concurrent |
| M10 | InvalidJson/Unreadable unchanged bytes | `rotate_for_test` |
| M11 | Unsafe home all write paths | write-back + agent |
| M12 | macOS same-process Busy; lock recreate; inode/path abort | lock tests |
| M13 | Same home dual FakeKeychain cache isolation | `StoreReader` |
| M14 | rr3 #1–#6 closed with tests | map above |

Each protocol step has injected crash/fail test (M2–M4, M5, M7).

---

## Security gates (unchanged holds)

- **Palmer explicit written spec approval** required.
- After **approved plan**, implementation merged only with: independent **read-only adversarial reviewer ≠ author** on fresh diff; supervisor full diff; **GATES_FAILED=0**, **ALL_KILLED** (timeouts count as failure), green CI; **no PR merge** before security signoff **and** R-8a policy; merge via `origin/main` rules as today.
- rr3 security defects **cannot be waived** by rationale; each closed in code + tests.

---

## Self-review

- Lock timeout: strict Busy, no lease-only adopt (removed).
- Pending before generic paths; composite lock before pending restore in diagram.
- RefreshInFlight before POST; ambiguous → reauth; no silent POST retry.
- Home owner via plugin `list()` + secret scan — **no hard blocker** if M1 passes; else escalate to Palmer.
- Fingerprints full tuple + store location; expired adopt = commit + refresh-needed error.
- No “encrypted” claim; protected account secret store per existing host contract.

---

## ─── Written Spec Approval Required ───

**STOP.** No implementation plan until **Palmer’s explicit written spec approval**.
