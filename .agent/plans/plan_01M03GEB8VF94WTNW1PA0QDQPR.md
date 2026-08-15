# Repair payment-attempt lifecycle boundaries

This change resolves the findings from the comprehensive review of commits since `v0.2.0`. The observable result is that an idempotent replay returns its durable canonical result after mutable subscription state changes, a prepared host charge cannot bypass boundary-aware readiness resolution, and host-charge cleanup continues making progress when early candidates are ineligible or locked. The work follows the repository security baseline in `docs/security/threat-model.md`; it changes payment orchestration but does not add a new authentication, authorization, credential, or financial-retention boundary.

## Progress

- [x] Reproduce the findings from source and identify their shared lifecycle boundaries.
- [x] Introduce one phase classifier that distinguishes resumable prepared attempts from canonical replays.
- [x] Route host-charge readiness through explicit unreserved and durable-reservation stages.
- [x] Replace fixed-prefix cleanup selection with a durable, skip-locked reconciliation claim.
- [x] Add lifecycle and cleanup regression tests, including deletion-boundary coverage.
- [x] Run focused checks and commit each behavioral slice separately.
- [x] Run repository formatting, Clippy, test, SQLx, contract, and Jig work gates.

## Surprises & Discoveries

- The durable ledger and provider-boundary model are sound. The defects arise where orchestration phase is represented by call ordering and repeated predicates rather than a named abstraction.
- Recovery already expires and reloads stale prepared work before deciding whether it can resume. Payment-method replacement has the same helper, but its direct reservation replay path did not use it.
- Host cleanup limits the candidate query to 100 rows before semantic eligibility is known. Rows skipped by the target callback remain at the front forever, so the query limit accidentally acts as a permanent work horizon.
- Exact reconciliation already establishes `updated_at` as the durable claim timestamp. Reusing that contract avoided a schema-only lease column and made host cleanup consistent with the existing account reconciliation envelope.
- `HostChargePaymentResult` is large enough that the readiness outcome needs boxed indirection to satisfy the workspace's Clippy policy; this does not change the public result type.

## Decision Log

- Decision: Classify every loaded attempt as either `ResumePrepared` (`pending` with no `submitted_at`) or `ReturnCanonical` (submitted or no longer pending). Only the prepared phase may be checked against mutable replay context. Rationale: idempotency owns immutable request identity; mutable state gates a new provider submission, not retrieval of an already durable result.
- Decision: The direct payment-method replacement reservation path will expire and reload stale prepared attempts before applying the same phase classifier used by preflight. Rationale: callers should not need to observe an undocumented ordering contract between preflight and reserve.
- Decision: Host-charge readiness will have explicit pre-reservation and durable-reservation entry points. The former may reject without a ledger row; the latter must resolve deterministic readiness outcomes against the durable attempt before returning.
- Decision: Cleanup will atomically claim eligible candidates with `FOR UPDATE SKIP LOCKED` by advancing only their reconciliation scheduling timestamp, then process targets one by one. Financial outcome fields remain unchanged until target and attempt transitions commit together. Rationale: a claim separates scan progress from successful transition count and makes crashes retryable after a bounded delay.
- Decision: Host target callback errors remain batch errors, while row-lock contention is candidate-local and does not prevent later candidates from being attempted.

## Outcomes & Retrospective

Implemented and verified in four commits:

- `d266775` separates canonical replay from prepared resumability and removes the payment-method reservation ordering dependency.
- `b0e2fe7` makes host readiness explicitly unreserved or prepared and adds the prepared deterministic-error regression.
- `abce47d` gives host cleanup durable skip-locked claims, progress ordering, and candidate-local lock handling.
- `107b339` adds the deletion blocker matrix for every attempt kind and submission phase.

Focused replay, host application, cleanup-progress, and deletion tests pass. `jig.fmt_check` passed with receipt `receipt_01M03HXXM9GKPFF5RYQ6J8MQ57`; workspace Clippy passed with `receipt_01M03J5WWT6QMRSMT35Z2MYV8H`; and the fresh work-check batch `receipt_01M03J4VM33KSXJT7RJ6AZWYPV` records passing contract, full workspace tests, and SQLx metadata gates. The Jig session closed successfully. No database schema or public Rust API changed, and no known review finding remains open.

## Context and orientation

Payment-attempt persistence lives under `crates/syrup-rail-postgres/src/attempts`. Recovery and payment-method replacement preflights load an attempt by idempotency key, expire stale prepared work, and decide between replay and a resumable reservation. `crates/syrup-rail-postgres/src/subscription_billing_service/host_charge.rs` coordinates application-facing host charges around a durable provider-submission boundary. `crates/syrup-rail-postgres/src/host_charge_reconciliation.rs` scans durable host-charge attempts and invokes the host-owned target resolver before transitioning both sides atomically.

A **prepared** attempt is pending and has no provider-submission timestamp. It can be resumed only while its snapshotted mutable billing context remains valid. A **canonical replay** is any submitted or non-pending attempt. Its immutable request must still match the repeated command, but its result must not be invalidated by later subscription changes. A **reconciliation claim** is a scheduling-only update that prevents repeatedly selected semantic skips from monopolizing a bounded batch.

Read `agent-map.md`, `crates/syrup-rail-postgres/AGENTS.md`, and `docs/security/threat-model.md` before editing these paths. Versioned schema artifacts are not changed by this work.

## Plan of work

First, add the phase classifier beside the shared attempt persistence helpers and replace path-specific status/submission predicates. Change recovery and payment-method replacement so locked canonical attempts replay immediately after immutable command matching, while only prepared attempts consult mutable subscription context. Make direct payment-method reservation perform stale expiry and the same decision.

Second, restructure host-charge orchestration so only work without a durable reservation runs the pre-reservation readiness/admission stage. Once a reservation exists, run exactly one durable-reservation readiness stage that translates deterministic readiness outcomes through the existing attempt-resolution boundary. Preserve transient unavailability as retryable pending work.

Third, change host cleanup selection into an account-scoped claim transaction using `FOR UPDATE SKIP LOCKED`, a bounded retry interval, and ordering that prioritizes unclaimed work. Continue after candidate-local SQL lock contention. Keep the target transition and attempt resolution in their existing per-candidate atomic transaction.

Finally, add tests for terminal replay after mutable state drift, direct stale payment-method reservation, prepared host readiness failures, more than one batch of semantic skips, a locked oldest candidate, and stale-attempt deletion thresholds across attempt kinds and statuses.

## Concrete steps

From `/home/aa/Documents/syrup-rail`:

1. Edit the shared attempt modules and focused attempt tests. Run the relevant `cargo test -p syrup-rail-postgres ...` filters and `scripts/jig check fmt --plan-id plan_01M03GEB8VF94WTNW1PA0QDQPR`, then commit the replay slice.
2. Edit `subscription_billing_service/host_charge.rs` and add a focused child test module. Run its tests and formatting, then commit the readiness slice.
3. Edit host reconciliation and its tests/docs. Run its focused tests and formatting, then commit the cleanup slice.
4. Add the stale-deletion matrix test and commit it as a test-only slice if it is not naturally owned by an earlier behavior change.
5. Run `scripts/jig work check --plan-id plan_01M03GEB8VF94WTNW1PA0QDQPR`, `scripts/jig check fmt`, `scripts/jig check clippy`, `scripts/jig check test`, `scripts/jig check sqlx`, and `scripts/jig check contract`. Capture evidence with `scripts/jig work evidence` and finish with `scripts/jig work gates` and `scripts/jig work finish`.

Success means repeated commands never resubmit canonical attempts or conflict merely because later mutable state changed; prepared host readiness failures durably resolve exactly once; cleanup reaches eligible work beyond skipped or locked prefixes; all required gates pass; and the user-owned `.agent/0.2.1-bug-findings.md` remains untouched.

## Validation and acceptance

Focused regression tests must demonstrate both state and call-count behavior where relevant: canonical replay returns the existing attempt without provider work, deterministic prepared-host readiness produces a terminal attempt without provider submission, transient unavailability leaves the prepared attempt retryable, cleanup can process a valid row after at least 100 semantic skips, and a locked oldest row does not abort or starve a later valid row.

The final acceptance suite is the repository contract: formatting, Clippy, backend tests, SQLx metadata verification, contract checks, and Jig gates. Review `git diff` and `git status` after every commit so generated state and unrelated user files are not accidentally included.

## Idempotence and recovery

All commands are safe to rerun. The code changes are forward-only source changes and require no schema rollback. If a focused test exposes a partial claim, wait for the bounded retry interval or set the test fixture timestamp back before rerunning; production claims become eligible automatically. If a commit is interrupted, inspect the index and worktree rather than resetting, preserve append-only `.agent/state/*.jsonl`, and continue from the last completed progress item.

## Interfaces and dependencies

No public Rust API or database schema changes are planned. The shared phase classifier is crate-internal. Host readiness continues using `GatewayReadiness` and the existing `OutcomeResolutionBoundary`. Cleanup continues using `HostChargeTargetStore`; only its scheduling behavior changes. SQL remains runtime-checked through SQLx and must pass the committed metadata gate.
