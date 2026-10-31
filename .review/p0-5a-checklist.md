# P0-5a checklist

Reference revision: 7428f3229, plus 80857a66c for runsOutAt. Each item has one named test. No test may use the network.

## Q1: quota model

- [ ] q1a: Quota serializes provider, name, kind, plan, user, windows, balance, error, asOf, readDeferred, readAt, until, renew, lastServedAt, last, and alias with reference camelCase names and omission rules; kind accepts subscription, plan, and balance. Test: `q1a_quota_json_uses_reference_names_and_omissions`.
- [ ] q1b: QuotaSpan serializes unlimited, name, used, remaining, resetsAt, display, text, amount, limit, unit, pool, runsOutAt, runsOut, and balance with reference names and omission rules. Test: `q1b_quota_span_json_includes_runs_out_fields`.
- [ ] q1c: A report derives percent remaining as max(0, 100-used), keeps explicit absolute reset and run-out times distinct, and never invents resetsAt from runsOutAt. Test: `q1c_report_clamps_remaining_and_keeps_run_out_distinct`.
- [ ] q1d: Plugin count windows convert to amount/limit/unit spans and retain model scope and non-blocking metadata for routing without adding non-reference JSON fields. Test: `q1d_plugin_windows_convert_without_losing_routing_metadata`.

## Q2: absolute reset and term text

- [ ] q2a: A relative reset is converted from the report clock to an absolute resetsAt; an already absolute reset wins. Test: `q2a_relative_reset_uses_injected_clock`.
- [ ] q2b: ResetClock prints HH:MM today or earlier, tomorrow HH:MM next day, weekday HH:MM within days 2-6, and month/day HH:MM otherwise. Test: `q2b_reset_clock_uses_calendar_day_buckets`.
- [ ] q2c: PlanTerm prints renews/expires/until plus local month/day for auto/off/unknown renewal, and empty text with no end. Test: `q2c_plan_term_covers_renewal_modes`.

## Q3: cache and read outcomes

- [ ] q3a: A successful nonempty read replaces the cache, records readAt, persists quotas.json atomically, and a reloaded store exposes the reading asOf its original read time. Test: `q3a_successful_read_persists_and_reloads_as_of`.
- [ ] q3b: A transient failed read falls back to the last good reading with asOf and readDeferred=false; a non-transient/auth failure stays an explicit error instead of being hidden. Test: `q3b_failed_read_distinguishes_cached_fallback_and_error`.
- [ ] q3c: A deferred/unsupported read makes no fetch, uses the last good reading when present, marks readDeferred=true, and is not a failure. Test: `q3c_deferred_read_is_not_a_failure`.
- [ ] q3d: A kept report returns cached/current readings without invoking the reader, including after restart; a fresh report invokes the reader. Test: `q3d_kept_report_never_fetches`.
- [ ] q3e: A cached window whose reset passed counts renewed for allowance, while its report remains dated and says the current allowance is unknown. Test: `q3e_expired_cached_window_is_not_still_exhausted`.

## Q4: provider order

- [ ] q4a: Reports use configured provider order, preserve card order within each provider across subscription/plan/balance groups, put unknown providers last, and do not mutate the cached order. Test: `q4a_provider_order_is_stable_and_non_mutating`.

## Q5: snapshot API and scoping hook

- [ ] q5a: Authenticated GET /v1/hofund/quotas returns `{object:"list",data:[...]}`; kept=1 selects the no-fetch report and the normal route selects a fresh read. Test: `q5a_snapshot_route_supports_kept_and_fresh_reads`.
- [ ] q5b: The scope hook receives the caller identity; default allows every card, while a scope filters providers and case-insensitive users on both fresh and kept snapshots without mutating the source. Test: `q5b_scope_hook_filters_snapshot_cards`.
- [ ] q5c: Snapshot routes use the gateway's normal bearer/x-api-key authentication and never expose caller credentials in errors or JSON. Test: `q5c_snapshot_requires_gateway_authentication`.

## Q6: history model, sampling, retention, and API

- [ ] q6a: A new readable window records at/readAt, percent left rounded to 2 decimals, optional start and resetsAt in UTC seconds; usernames are lowercased and provider/window lines sort by name. Test: `q6a_history_records_normalized_points`.
- [ ] q6b: Failed, cached/asOf, empty-name, unlimited, non-blocking, and non-finite readings are not new history points; repeated cached readAt values do not duplicate points. Test: `q6b_history_excludes_non_new_or_undrawable_readings`.
- [ ] q6c: Points inside 30 seconds replace the previous same-cycle point; within a 5-minute gap the last point moves; a flat run keeps only its first and last point. Test: `q6c_history_samples_near_and_flat_points`.
- [ ] q6d: Passed/moved resets, fixed-deadline refills over 5 points, and reset-presence changes start cycles; rolling reset movement and small corrections do not, and cycle boundaries survive close sampling. Test: `q6d_history_detects_cycle_boundaries`.
- [ ] q6e: History prunes points older than 45 days and keeps at most 6000 newest points per window, dropping empty lines/accounts. Test: `q6e_history_enforces_retention_and_point_cap`.
- [ ] q6f: History range queries accept positive fractional days up to 45, otherwise use 45 days, include one predecessor at the range edge, and filter provider/user case-insensitively for user. Test: `q6f_history_range_and_filters_match_reference`.
- [ ] q6g: Authenticated GET /v1/hofund/quotas/history returns the list envelope, honors days/provider/user, and applies the same scope hook as snapshots. Test: `q6g_history_route_filters_and_scopes`.

## Q7: quota CLI and wait

- [ ] q7a: `hofund quota [filters...] --json` prints a JSON array of the Q1 model, including runsOutAt/runsOut and last-served fields, and filters provider/name/kind/alias case-insensitively. Test: `q7a_quota_json_and_filters`.
- [ ] q7b: Text output has one aligned title/kind line per card, window used%/display/reset text, balance/error/cache age, plan term and alias; no runs-out column is invented. Test: `q7b_quota_table_matches_reference_cells`.
- [ ] q7c: Empty unfiltered output explains what can appear; an unknown filter is an error; an exhausted/invalid reset time is rendered explicitly rather than treated as a fresh reset. Test: `q7c_quota_empty_unknown_and_expired_output`.
- [ ] q7d: `quota wait` resolves provider/name/account/provider-account targets, returns immediately for any available account, and narrows correctly to one account; ambiguous and unknown targets are usage errors. Test: `q7d_wait_resolves_targets_and_available_allowance`.
- [ ] q7e: A used-up wait polls shortly after the soonest reset with 30s margin, clamped to 1-10 minutes, retries unknown reads with 1/2/4/8/10-minute backoff, and honors the deadline with a final read. Test: `q7e_wait_schedule_backoff_and_timeout`.
- [ ] q7f: Cached/asOf or error readings do not prove availability; non-blocking windows do not block; quiet suppresses progress; timeout exits 1 and usage/target errors exit 2. Test: `q7f_wait_readability_blocking_quiet_and_exit_codes`.

## Q8: last served

- [ ] q8a: Successful served request rows project onto provider/account cards by member identity, omit failures and entries older than 30 days, survive through the persisted usage ledger, and leave never-served fields absent. Test: `q8a_last_served_uses_successful_rows_within_30_days`.
- [ ] q8b: The newest lastServedAt sets last=true on every tied card only; a no-user provider card uses the latest of its accounts. Test: `q8b_last_served_marks_latest_and_ties`.

## Q9: router-facing allowance view

- [ ] q9a: Allowance exposes each member/window's share left as max(0, 100-used), with amount/limit/unit when known, and treats an expired reset as 100% left. Test: `q9a_allowance_reports_share_left_and_renewal`.
- [ ] q9b: Model-scoped windows apply only to their model, non-blocking windows never exhaust routing, and unknown/failed members have no fabricated allowance. Test: `q9b_allowance_respects_model_scope_blocking_and_failures`.
- [ ] q9c: The fullest applicable live window is the highest used share, ties choose the later reset, and its renewal view exposes all applicable reset deadlines in order. Test: `q9c_allowance_fullest_and_renewal_order`.

## Q10: documentation

- [ ] q10a: CHANGELOG Unreleased, feature-parity rows 3.8/3.9, architecture/dependency records, and magpie-comparison record the landed scope and deferred plan/balance work accurately. Test: `q10a_docs_record_quota_surfaces_and_deferred_work`.

## Deferred to P0-5b

- Kimi monthly window.
- Plan windows from planquota.go and plan keys weighed by plan windows.
- Balance URLs, balance splitting, and shared balances.
- Hourly signed-in account model-list refetch.
- Quota cards at RemoteCardsPath/RemoteRefreshPath and remote quota-card merging.
- Quota refresh CLI.
- Until/Renew population from vendor adapters (the Q1 model and text support them).
