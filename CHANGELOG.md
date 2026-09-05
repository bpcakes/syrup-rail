# Changelog

All notable changes to the Syrup Rail crates are documented in this file.

## [Unreleased]

### Changed

- Derive NMI approval signals before duplicate/status reduction, and preserve
  them through identity quarantine and text truncation. `PaymentOutcomeParts`
  now includes `approval_evidence`; update constructed fixtures accordingly.
- Make manual-review protection depend only on typed evidence, preserve provider
  text during negative exact queries, and keep empty reservations recoverable.
  Indeterminate mutation-error details remain unclassified; proven non-submission
  carries no approval signal. Protocol-level indeterminate errors, processor
  duplicates, missing decisions, and text-only pending states likewise remain
  protected. Unidentified reconciliation observations cannot erase earlier
  approval signals; payment-method timeout cleanup preserves provider evidence.
- Preserve response-text-only payment-method-update evidence during manual closure
  and retain its existing condition instead of fabricating a failed provider state.
- Move conservative approval-signal interpretation into the NMI raw parser and
  translate its summary at the adapter boundary. Numeric
  response-code aliases such as `0100` and `+0100` now retain the same financial
  review protection as `100`, without promoting unknown decisions to approved.
- Add `ProcessorApprovalEvidence` and preserve it through attempt application,
  reconciliation, processor-charge recording, and external-reversal attestations.
  Deprecate the historical raw-string gateway approval helpers.

### Migration

- Require schema v6 and `assert_runtime_schema_v6_compatible` for this workspace.
  Apply the complete install or the forward-only v5-to-v6 cutover with billing
  writers stopped. Shipped v1-v5 artifacts are unchanged. Retained provider evidence is
  marked `Unclassified`; only entirely empty attempt evidence is initialized as
  `Absent`. Legacy local query notes stay protected because v5 could overwrite
  provider evidence with those notes. No provider strings are parsed.
- Third-party adapters must attach explicit approval signals using
  `ProcessorEvidence::with_approval_evidence`. The compatibility constructor
  marks empty observations `Absent` and nonempty observations `Unclassified`.
  Unclassified evidence always blocks manual failure and
  cannot create a new immutable pending charge without a structured signal.
  Raw-observation exact replay leaves already-recorded classifications unchanged.

## [0.6.0] - 2026-09-01

### Fixed

- Make the retained schema-v4 runtime assertion reject incompatible live
  external-reversal resolution tuples in the same read-only snapshot as its
  catalog check. Hosts validating v4 while staging the required schema-v5
  cutover can enable the `schema-contract-test-support` feature to use it.
- Coordinate subscriber billing-data scrubbing with approved payment-method
  writers through the deployed payment-method advisory-lock identity. A
  concurrent approval can no longer restore mutable attempt or payment-method
  data while the scrub runs.

### Breaking

- Gate `assert_runtime_schema_v4_compatible` behind
  `schema-contract-test-support`. Production startup now uses
  `assert_runtime_schema_v5_compatible` after the required v5 cutover.

- Replace the NMI client's independent sale source, vault action, stored-
  credential, and currency fields with the closed `SaleIntent` contract.
  Direct raw-client callers must construct one of the five supported intents;
  code migrating historical field combinations can use
  `SaleIntent::from_legacy_parts`. NMI sales remain fixed to USD.
- Replace the independent `disposition` and `access` fields on
  `BillingEvent::SubscriptionPaymentFailed` with one
  `SubscriptionPaymentFailureOutcome`. Consumers can obtain the compatibility
  projections through `outcome.disposition()` and `outcome.access()`; the host
  integration example preserves the existing V1 outbox JSON shape.
- Replace the raw `ExternalReversalAttestation::new` constructor with a typed
  `ExternalReversalResolution` input. Code that reconstructs legacy raw tuples
  must use the fallible `ExternalReversalAttestation::from_legacy_parts` and
  handle `ExternalReversalResolutionError` instead of relying on unchecked or
  panicking construction.

### Changed

- Count invalid or conflicting gateway lifecycle evidence in reconciliation
  quarantine totals. Reconciliation summaries count only newly staged
  evidence, including a refused host-target reversal; redelivery of an already
  durable pending row does not increment `staged` again.
- Add `SubscriptionLifecycle` as the validated status, billing-period, and
  payment-schedule construction path. PostgreSQL subscription hydration now
  rejects contradictory rows, while `Subscription::new` remains available as
  the flat compatibility constructor.
- Model subscription discount-claim lifecycle and grant revocation audit facts
  as closed state values while retaining the legacy constructors and getters as
  compatibility projections.
- Add `GatewayAccountIdentity` and the compatible
  `GatewayResolver::resolve_identity` entry point so account identity travels
  intact across persistence and resolver boundaries.
- Separate canonical live payment-attempt request construction from opaque
  persisted fingerprint rehydration. New requests use
  `PaymentAttemptRequest::canonical`; persistence codecs use
  `PaymentAttemptRequest::from_persisted_parts`, while `new` remains available
  as a compatibility wrapper for callers carrying durable fingerprints.
- Record provider-rate-limit readiness failures without inventing a
  `gateway_condition` of `failed`. The typed provider-rate-limit resolution
  code remains the durable reason for the failed attempt.
- Recheck automatic-renewal account mode after reservation while retaining the
  final capability-consumption check immediately before sale. Determinate
  post-reservation failures resolve the canonical attempt without submission;
  a final transport failure remains a typed not-submitted renewal result.
- Interpret each NMI transaction report's lifecycle actions in one bounded
  pass while preserving action precedence, success handling, refund economics,
  and diagnostic provenance.
- Use `expires_at` as the sole pending lifecycle-evidence clock. Reconciliation
  no longer repeats the unactionable-candidate query or writes inert check
  counters; the existing bounded expiry cleanup behavior remains unchanged.

### Migration

- Version 0.6.0 requires PostgreSQL schema v5. Before scheduling downtime, run
  `schema/v5/preflight_from_v4.sql` to measure retained external-reversal
  attestations and count incompatible resolution tuples. When blockers exist,
  run `schema/v5/audit_incompatible_attestations_from_v4.sql` through an
  authorized operator process to identify the affected internal rows without
  exposing unnecessary provider or presentation evidence. Rehearse the exact
  upgrade artifact on representative data, drain schema-v4 billing traffic,
  stop every v4 writer, and apply `schema/v5/upgrade_from_v4.sql` in one
  host-owned transaction. Its validated CHECK replacement scans the retained
  table under `ACCESS EXCLUSIVE`; size the maintenance window and configure
  deployment timeouts from the rehearsal rather than an assumed universal row
  limit. Investigate blockers through an audited host process, never by
  bypassing the constraint or silently rewriting financial evidence. After
  commit, start 0.6.0 with `assert_runtime_schema_v5_compatible`; do not restart
  a v4 writer.

### Developer experience

- Consolidate initial-attempt row locks, transient SQLSTATE recognition, local
  timeout execution, audit-reason validation, and processor-charge role decoding
  behind private helpers while preserving public contracts and workflow policy.
- Keep release-wrapper fixtures aligned with exact internal dependency pins
  and verify that non-exact pins are rejected before packaging.
- Check documentation and doctests with default features as well as all
  features, and correct renewal documentation to require the schema-v5 startup
  assertion.
- Share the current schema install selection between integration fixtures and
  the independent SQLx metadata gate so both validate schema v5.
- Make clean release preflight work under Bash 3.2 through 4.3 by avoiding
  nounset expansion of an empty optional-argument array. CI now exercises clean
  and `--allow-dirty` packaging under both current Bash and macOS Bash 3.2.

### Internal refactoring

- Narrow shared approval parking to initial enrollment, recovery, and renewal;
  payment-method replacement retains its separate zero-value workflow.
- Clarify lifecycle outcome counts versus newly staged reconciliation rows and
  the feature required to validate schema v4 while preparing the v5 cutover.

## [0.5.2] - 2026-09-04

### Fixed

- Serialize lifecycle-quarantine alert claims per gateway account so concurrent
  claimants cannot both emit an operator alert within one cadence window.
  Contending same-account callers now return `None` without waiting, while
  claims for different accounts remain independent.

## [0.5.1] - 2026-09-02

### Added

- Expose call-scoped payment-result diagnostics through
  `observation_diagnostics()` and the corresponding `with_` and `into_parts_`
  methods. The name distinguishes the latest gateway observation from the
  authoritative durable attempt status it annotates.

### Fixed

- Treat a blank or JSON `null` foreground transaction identifier like an omitted
  identifier for every non-approved NMI decision. Declines remain `Declined`;
  response codes `300`, `410`, `411`, `460`, and `461` remain `Failed`; and
  processor-error codes `400`, `440`, and `441`, communication-error codes `420`
  and `421`, and otherwise unresolved generic provider errors remain `Unknown`
  because NMI does not guarantee that they had no financial effect. Approved
  responses still fail closed through the public operation's required-identity check,
  while any independently valid customer-vault identifier remains available as
  reconciliation evidence and cannot be refined into approved evidence.
  Conflicting identity aliases clear the identity bundle while preserving an
  otherwise determinate decline or failure. Exact-query XML still rejects
  multiple records and binds every selector supplied by the caller; an
  order-only result may retain a coherent non-approved decision after its
  matching order ID is bound, while an unusable response transaction ID remains
  quarantined and diagnosed. The exact-query parser distinguishes a genuinely
  absent transaction ID from a present but invalid ID before identity-bundle
  quarantine, so it does not mislabel rejected evidence as missing.
- Preserve every payload-free NMI payment anomaly across the provider-neutral
  gateway boundary. Missing, malformed, conflicting, and unrecognized decision
  or identifier evidence—including values rejected by stricter provider-neutral
  identifier admission—no longer disappears before host policy can inspect
  `GatewayPaymentOutcome::diagnostics()`. A missing identity may leave its valid
  sibling available for reconciliation, but an invalid, conflicting, or
  adapter-rejected identity quarantines the complete NMI identity bundle so a
  parseable sibling cannot later become authoritative. Identity quarantine does
  not erase an otherwise determinate decline or failure.
  Every reconciled payment workflow now compares observed identities with the
  current durable attempt under its canonical application lock. Conflicts keep
  the attempt unresolved without overwriting its forensic identity bundle;
  conflicting approvals are retained in the processor-charge ledger as
  reconciliation-required observations instead of mutating subscription or
  host-target state. Approvals never restore a missing or quarantined identity
  from older evidence. Compatible non-approved observations may retain a
  durable identity, but their decision and descriptor bundles come entirely
  from the current observation so evidence from separate responses cannot
  synthesize approval. When an unanchored partial identity must be discarded
  to preserve that bundle boundary, the result reports the rejected field as a
  typed observation conflict instead of silently losing it.
  Already-terminal attempts compare the raw observation rather than a
  backfilled replay and report identity conflicts without rewriting the durable
  winner; a conflicting late approval is recorded for external reversal, and
  replaying an older reconciliation-required charge after the attempt becomes
  approved promotes it into that reversal queue.
  A contradictory payment-method reference for the same known processor
  transaction is reported as observation metadata conflict without rewriting
  or reclassifying an already-applied charge. For an unresolved attempt, the
  approval still enters the charge ledger; an existing immutable charge with
  contradictory metadata fails visibly rather than making the approval
  disappear. Late approvals after a durable decline or failure carry those
  identity-conflict diagnostics through the existing reversal-retention path.
  Late-approval parking preserves any established attempt observation instead
  of replacing it with sparse or conflicting identity fields, while retaining
  the raw approval in the charge ledger. Reconciled approvals use the same transaction-local
  charge-observation and parking core as foreground approvals, and exhausted
  host-coordinator retries fall back to the same locked pool-backed
  reconciliation policy instead of losing or blindly writing the latest
  approval; that fallback cannot demote an already applied charge into the
  external-reversal queue.
  Diagnostics are deduplicated in a canonical order and have set semantics;
  route with `has_diagnostic()` rather than assigning meaning to sequence.
- Make payment certainty a monotonic invariant of `GatewayPaymentOutcome`:
  malformed, conflicting, unrecognized, missing, indeterminate, unmapped, or
  processor-duplicate decision diagnostics force `Unknown` from any reported
  status. Replacing diagnostics cannot restore an earlier terminal status.
  Identity diagnostics remain field-usability facts and now quarantine their
  corresponding fields on `GatewayPaymentOutcome`, so workflow-specific
  identity requirements cannot accidentally apply diagnosed evidence. A
  missing identity leaves an approval visible for workflow-specific parking;
  an invalid or conflicting identity makes an approval `Unknown`. Neither kind
  converts an explicit decline or failure into reconciliation work.
- Preserve generic provider-error provenance when malformed or conflicting
  sibling evidence prevents a reported failure from winning the complete NMI
  decision reduction.
- Pin the four coordinated Syrup Rail crates to the exact same internal release
  version. Payment-diagnostic fallback is deliberately conservative, so Cargo
  must not silently combine different patch-level policy vocabularies. The
  release preflight verifies the exact `=VERSION` requirements.

### Deprecated

- Deprecate the 0.5.0 payment-result `gateway_diagnostics()` names in favor of
  their observation-specific replacements. The compatibility methods continue
  to forward without changing behavior.

### Action required for hosts

- Remove host-side parsing or reclassification of NMI response strings when
  upgrading. Use `GatewayPaymentOutcome::status()` as the authoritative
  decision and typed `GatewayPaymentDiagnostic` values for anomaly routing.
  The diagnostic enum remains non-exhaustive; retain a wildcard match arm.
- Audit custom `Gateway` adapters that call `with_diagnostics()`. Decision-
  certainty and processor-duplicate diagnostics now make `Approved`,
  `Declined`, or `Failed` effectively `Unknown`, and that downgrade is sticky.
  Identity diagnostics do not reclassify determinate non-approved decisions.
  They remove the corresponding unusable identity from the returned outcome;
  retain raw provider evidence only at the adapter's protected diagnostic
  boundary, not as application authority.
  Route exclusively on the returned `status()` rather than a status captured
  before diagnostics were attached.
- Expect processor-error codes `400`, `440`, and `441` plus otherwise unresolved
  generic provider errors to enter exact reconciliation instead of terminal
  failure handling. NMI does not document an empty exact-query result as final,
  so a stale empty result remains operator review for sales and renewal/dunning
  attempts rather than consuming a determinate failure.
- Monitor the age and volume of `attempt_review_page` results and drain them
  through the privileged operator workflow. Syrup Rail exposes bounded review
  and resolution primitives, but the host owns scheduling, alerting, operator
  authorization, and presentation. After the operator has independently
  established that an attempt with no gateway reference and no approval
  evidence had no financial effect, use `fail_review_required_attempt`; rows
  carrying a gateway reference or approval evidence deliberately remain open
  for stronger reconciliation or reversal evidence.

## [0.5.0] - 2026-08-28

### Fixed

- Remove the global `dup_seconds=0` hardcode from NMI sale encoders. The new
  `ClientFactory::client_with_duplicate_check` constructor requires each
  account client to choose `ProcessorConfigured` or a validated positive
  `Window` explicitly. Zero is not a valid window and is no longer modeled or
  sent by any constructor.
- Keep payment certainty order-independent when NMI repeats numerically
  equivalent decision fields. Only exact textual canonical response code `301`
  proves a pre-processing rate limit; whitespace, numeric JSON, contradictory
  non-2xx payment evidence, unproven HTTP 400 envelopes, and the undocumented
  v5 HTTP 422 status remain indeterminate. Classic query/report HTTP 422 keeps
  its permanent invalid-request classification. Unknown, malformed,
  conflicting, noncanonical, or outer-status-mismatched `status` fields also
  remain indeterminate, while generic canonical HTTP metadata does not
  masquerade as payment evidence. Form-encoded evidence returned from a v5
  endpoint, unknown or extended JSON error objects, and non-empty top-level JSON
  arrays also fail closed. The pre-processing `301` proof accepts only NMI's
  closed documented field set. Equivalent duplicate fields retain a
  deterministic provider-observed spelling rather than a synthetic token.
- Preserve provider-neutral gateway diagnostics on foreground subscription and
  host-charge payment results. The exact response code remains durable;
  foreground diagnostics are not separately persisted for later replay and do
  not participate in equality of the durable result.

### Deprecated

- Deprecate `ClientFactory::client`. Releases through 0.4.0 sent
  `dup_seconds=0`; the deprecated constructor now corrects that invalid
  override by using the processor-configured policy.
- Deprecate the compatibility `GatewayPaymentOutcome::into_parts` and
  `SubscriptionEnrollmentPaymentResult::into_parts` methods because they drop
  diagnostics. Use their diagnostic-preserving replacements.

### Maintenance

- Update the locked test-tooling dependency from yanked `chacha20` 0.10.1 to
  0.10.2.

### Action required for hosts

- Migrate every NMI account client to `client_with_duplicate_check`. Use
  `ProcessorConfigured` to omit `dup_seconds`; select a positive `Window` only
  after verifying that the account permits that per-transaction override.
  Retaining the processor policy trades additional defense in depth for
  possible heuristic rejection of a legitimate later payment. Duplicate
  response code `430` remains `Unknown` and must be reconciled; NMI does not
  document it as proof of non-submission. It now carries the payload-free
  raw-client `DuplicateTransactionAtProcessor` diagnostic, which the adapter
  maps to provider-neutral
  `GatewayPaymentDiagnostic::ProcessorReportedDuplicate` on
  `GatewayPaymentOutcome`, so hosts can route it without parsing provider text.
- NMI's sandbox does not exercise a payment processor, so validate the
  effective duplicate-check and merchant-override settings for each controlled
  pre-production processor account before rollout. Hosts that deny deprecation
  warnings must migrate from `ClientFactory::client` in the same change as
  upgrading the dependency. Keep the processor duplicate window shorter than
  the shortest normal billing or renewal interval and the host's
  replacement-charge interval, then wait out the window by default after an
  indeterminate attempt.
## [0.4.0] - 2026-08-27

### Added

- Allow hosts to require an exact gateway account mode for payment mutations.
  The service remains live-only by default; explicitly requiring test mode
  permits test-account mutations while rejecting an observed live account
  before submission.
- Add PostgreSQL schema v4 and its forward-only v3 cutover. Payment attempts
  and subscriptions now persist their required gateway account mode so neither
  prepared work nor future renewal authority can cross deployment modes.
- Require an opaque, mode-verified gateway capability at every supported
  low-level provider-submission boundary.
- Make `EntitlementQuery` and `EntitlementGuard` default to live paid
  subscriptions. Test workers select `Test`; trusted administrative tooling
  can opt into cross-mode reads with `across_gateway_account_modes`.

### Changed

- Require every low-level reservation constructor, plus direct
  `PaymentAttemptIdentity` construction, to state the trusted gateway account
  mode explicitly instead of silently choosing `live`. The public
  `Subscription::new` and `RenewalDispatch::new` constructors now take that
  mode, `RenewalDispatchPageCursor::new` now takes its optional mode filter,
  and `preflight_host_charge_in_transaction` now takes the service's required
  mode so it can skip host callbacks for wrong-mode prepared replay.
- Return terminal idempotent results across deployment-mode changes while
  rejecting only prepared attempts that could still reach the provider.
- Leave a prepared initial-enrollment attempt intact when a wrong-mode
  low-level admission caller reaches it, so the matching deployment can still
  complete the canonical work. Submission admission now validates the locked
  attempt first, so wrong-mode and already-admitted replays do not invoke the
  host offer callback.
- Include the subscription's required gateway account mode in renewal
  dispatches so hosts can route work to a matching service deployment. Add
  `due_renewals_page_for_mode` and `due_renewals_for_mode` so a mode-specific
  worker uses a dedicated mode-leading index before applying the bounded page
  limit. Renewal cursors now record their mode filter and reject cross-mode or
  filtered/unfiltered reuse.
- Remove the v4 cutover defaults after backfill so every current-schema
  payment-attempt and subscription writer must supply an explicit mode.
- Add the scope-neutral `GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS`
  constant. The incompatible 0.4.0 cutover removes the pre-existing
  renewal-specific name; migrate callers to the scope-neutral constant.
- Stage the schema-v3-to-v4 cutover so the large constraint-validation scan
  runs after the exclusive preparation lock is committed.
- Build the mode-leading renewal index in a dedicated non-transactional
  `index_from_v3.sql` stage with `CREATE INDEX CONCURRENTLY`, along with a
  covering replacement for the all-mode renewal index, before writers stop,
  instead of putting either table scan in the final billing outage.
- Publish the mandatory schema-v4 runtime boundary as Cargo-incompatible 0.4.0
  and raise the minimum `h2` version to 0.4.16.

### Action required for hosts

- Obtain explicit deployment-owner sign-off before starting the schema-v4
  prepare stage. Preparation opens a roll-forward-only availability-risk window
  in which a restarted v3 process cannot pass startup validation. Use a full
  billing maintenance window for all four stages by default. Keeping v3 writers
  active through the concurrent-build stages is an exception that requires
  written sign-off proving process restarts are prevented for the whole window.
- Rehearse NMI query-rate headroom for the deployment's mutation volume. The
  final mode verification adds one account-mode request to initial enrollment
  and prepared host-charge replay; other flows already performed that final
  query. Alert before rollout if the added control-plane traffic approaches the
  provider's throttle budget.
- Update test-deployment entitlement construction before upgrading:
  `EntitlementQuery::new` and `EntitlementGuard::new` now select live paid
  subscriptions. Every test worker that previously relied on these constructors
  without a mode must explicitly call
  `with_required_gateway_account_mode(GatewayAccountMode::Test)`; otherwise its
  test subscriptions are denied. Use `across_gateway_account_modes` only for
  trusted administrative tooling that intentionally owns both modes.
- Return `Applied` or `ExactReplay` from every successful
  `HostChargeTargetStore::apply_transition`. `StaleTarget` and `Unchanged` now
  fail closed for submitted declines as well as pre-submission releases and
  first-time approved/reversal paths. The sole exception is a `Paid` callback
  made while locking an already-approved canonical replay: after the attempt
  lock proves the original paid transition committed, a refused replay returns
  that canonical payment so a later legitimate reversal remains intact. Keep
  host transitions monotonic—return `StaleTarget` for a reversed target and
  never move it back to paid. A submitted attempt remains pending until lifecycle
  reconciliation or operator review if the host refuses the target transition;
  routine account/provider cooldown pacing also invokes
  `ReleasedBeforeSubmission` and returns `InvalidState` if refused. Audit this
  callback before deploying 0.4.0 to avoid stranded attempts. Make a host
  integration test that applies and exactly replays
  `ReleasedBeforeSubmission` for ordinary cooldown pacing a release gate; enum
  exhaustiveness forces a new match arm but cannot prove the adapter returns a
  durable outcome. If a refusal follows a concurrent canonical winner, the
  caller reloads and returns that canonical payment instead of reporting
  `InvalidState`.
- Alert on repeated `syrup_rail::gateway_cooldown` warnings for the same account
  or configuration. A rotated or removed identity intentionally skips its
  advisory cooldown, but repeated identity mismatches can reveal stale resolver
  configuration and leave that account without shared throttle protection.
  Treat an error on the same target as a schema/configuration invariant breach:
  provider-scoped cooldown storage is missing and the financial attempt is
  deliberately left unresolved.
- Alert on `syrup_rail::host_charge_target` warnings. They identify refused
  payment/release transitions whose financial attempt intentionally remains
  unresolved until host repair or explicit reconciliation.
- Partition production access explicitly if test- and live-mode subscriptions
  coexist in one database. Entitlement reads and guards default to live paid
  subscriptions; test workers must select `Test`, and trusted cross-mode tools
  must call `across_gateway_account_modes`. Host-issued grants,
  current-subscription reads, and billing portal reads remain mode-neutral.
- Treat subscriber/plan state as one aggregate across modes. A test-mode
  subscription or active grant blocks a second live enrollment, saved discount
  claims are shared, and cancellation/deletion blockers are mode-neutral.
  Entitlement guidance for an in-flight initial attempt is also mode-neutral,
  so it agrees with enrollment admission even when paid-subscription projection
  is mode-filtered. Use isolated databases/tenants or synthetic test subscribers
  when test activity must not affect a real subscriber's live billing lifecycle.
- Audit every custom `PaymentGateway` adapter before upgrading.
  `GatewayNotSubmittedError::Unavailable` was renamed to `NotTransmitted` so
  adapters must acknowledge that the variant now authorizes same-attempt,
  same-order replay and is valid only when the adapter proves that no request
  reached the provider. A possibly transmitted request must remain
  indeterminate; misclassification can duplicate a charge.
  The durable resolution code intentionally remains
  `gateway_unavailable_before_submission` for stored-data compatibility even
  though rows carrying it now certify the stronger `NotTransmitted`, same-key
  retryable contract.
- Implement the renamed `HostChargeTargetStore::ensure_submission_admitted`
  callback as a repeat-safe operation for the same attempt. A transient final
  mode query retains the claimed target, and same-key retry revalidates it
  through that callback before provider I/O. Repeating the callback for an
  attempt must produce no additional observable side effect: use keyed upserts
  for audit facts and do not increment counters or append duplicate audit rows.
- Keep enrollment offer callbacks within the documented lock order. Submission
  admission now locks and validates the attempt before invoking the callback;
  already-admitted replay and wrong-mode rejection skip it. The callback must
  use only the supplied connection, must not acquire attempt-ledger locks, and
  must not perform slow external I/O. Reservation may still invoke it before an
  attempt row exists.
- Keep `HostChargeTargetStore::preflight_target` side-effect-free. Terminal
  and review-required replay, plus a wrong-mode prepared replay, now resolve
  before that callback; a same-mode prepared replay still invokes it to
  revalidate target economics. Canonical review replay owns its retained
  economics, so changed one-shot command values do not replace it with an
  idempotency conflict. Canonical terminal and submitted in-flight replay use a
  lock-free snapshot path and do not re-check current host economics; unlike
  0.3, an in-flight replay can observe the canonical attempt as pending instead
  of waiting for its concurrent application transaction. The attempt identity
  remains authoritative.
  `HostChargeReservationDecision::IdempotentContender` now carries the current
  target snapshot, which hosts must return so the ledger can reject a changed
  same-key charge before gateway I/O.
- Handle the new `HostChargeTargetTransitionKind::ReleasedBeforeSubmission`
  variant by repeat-safely releasing a claimed target. It represents a
  determinate pre-submission failure, including infrastructure
  misconfiguration and an already-active mutation cooldown; `PaymentFailed`
  remains reserved for submitted outcomes.
- Update stale host-charge cleanup handling: the existing
  `fail_stale_unsubmitted_host_charges` path now emits
  `ReleasedBeforeSubmission` instead of `PaymentFailed`. Move any host dunning,
  notification, or retry logic for that never-submitted cleanup outcome to the
  new transition; `PaymentFailed` will no longer fire for it.
- Prepared host-charge mode mismatch now returns
  `Err(GatewayReadiness(Configuration))` after durably resolving and releasing
  the target, matching fresh host charges. Subscriber flows retain their
  existing `Ok(canonical failed payment)` surface for determinate readiness
  failures. Terminal same-key replay still returns the canonical payment.
- Handle post-reservation renewal readiness failures as
  `SubscriptionRenewalOutcome::NotSubmitted { payment, error }`. These failures
  were previously resolved and returned as `Noop`; schedulers should now record
  or classify the typed not-submitted error while treating the returned payment
  as the canonical durable result.
- Route each `RenewalDispatch` to a service whose configured required mode
  matches `RenewalDispatch::required_gateway_account_mode`. Mode-specific
  workers should scan with `due_renewals_page_for_mode` and keep that mode for
  the whole cursor chain; the unfiltered API is for a central router that owns
  both modes. Reservation also revalidates the durable subscription mode and
  fails closed on mismatch. The mode is write-once subscription authority;
  there is no supported API or migration that changes an existing
  subscription's mode. Route it to the matching deployment instead.
- Update exhaustive matches for the new
  `GatewayNotSubmittedError::{AccountModeMismatch, AccountModeVerification}`
  variants; the new `GatewayAccountModeChanged` variants on
  `SubscriptionEnrollmentReservationRejection`,
  `SubscriptionEnrollmentSubmissionRejection`,
  `SubscriptionRecoveryReservationRejection`,
  `SubscriptionPaymentMethodReplacementRejection`,
  `SubscriptionRenewalReservationRejection`, and `HostChargeReservationOutcome`;
  the new `RenewalStoreError::CursorModeMismatch`; the new
  `GatewayLifecycleApplyOutcome::HostTargetTransitionSkipped`; and the new durable
  `PaymentResolutionCode::GatewayTestReadinessFailedBeforeSubmission` value.
  Keep provider-mode details out of subscriber UI copy; typed low-level errors
  retain required and observed modes for protected operator diagnostics.
- Treat `GatewayLifecycleReconciliationSummary::skipped()` as retryable host
  reconciliation work. A refused full-reversal target transition now rolls
  back the attempt update, durably re-stages first-seen evidence before a
  provider cursor can advance, and continues the batch so unrelated reports
  still make progress. `GatewayLifecycleReconciliationSummary::staged()` now
  counts only newly inserted pending evidence; redelivery of the same unmatched,
  ambiguous, or host-refused report contributes zero instead of counting the
  already-durable pending row again.
- During the staged v3-to-v4 cutover, do not restart a schema-v3 process after
  `prepare_from_v3.sql` commits: its v3 startup assertion intentionally rejects
  the prepared catalog. Keep existing v3 writers running through validation,
  then run `index_from_v3.sql` outside an explicit transaction while they
  continue. Stop them only after the concurrent index is valid, then roll
  forward through `upgrade_from_v3.sql` and the 0.4 deployment.

### Removed

- Remove the schema-v3 runtime assertion from the ordinary production facade;
  schema-v3 validation remains available only to tests and migration tooling.

### Fixed

- Verify the required gateway account mode both before final admission and
  again at the provider-submission boundary. The first check avoids creating or
  admitting work when the account is already misconfigured; the mandatory
  second check prevents supported low-level submitters from carrying a stale
  observation across admission. NMI still exposes mode lookup and mutation as
  separate requests, so hard test/live isolation requires separate merchant
  accounts rather than coordination around one mutable account switch. Initial
  enrollments and prepared host-charge replays gain one provider query; a
  transient failure of that final post-admission query now atomically restores
  resumable enrollment, recovery, payment-method replacement, and host-charge
  attempts to prepared state. Automatic renewal retains its established
  terminal not-submitted handling because it does not resume prepared attempts.
- Centralize replay classification for durable attempts so terminal and local
  repair results remain canonical across deployment-mode changes, while every
  prepared attempt that could still reach the provider rejects a changed
  required mode consistently.
- Classify every not-submitted mutation once for its durable resolution,
  cooldown, retry safety, and host-target release. Determinate final
  account-mode failures now release a claimed host target together with the
  terminal never-submitted attempt, while transient unavailability restores
  the prepared attempt and retains its claim. Host-charge mutation
  throttles now persist the same account cooldown as subscriber mutations and
  release their terminal attempt's claimed target; determinate
  prepared-readiness failures and already-active cooldown gates likewise
  release their target atomically.
- Treat every transient `Unavailable` result that is provably not submitted as
  retry-safe for flows with prepared-attempt replay, regardless of whether it
  came from the mutation transport or the final account-mode query. Invalid
  adapter attempts to return caller-owned account-mode variants are normalized
  to a terminal malformed-contract error.
- Commit an observed provider/account throttle independently of canonical
  attempt and host-target resolution. Concurrent operator or reconciliation
  resolution, a stale host target, or a later host callback failure can no
  longer roll back cooldown protection for unrelated gateway work. A rotated
  or removed gateway identity skips the advisory cooldown with a warning
  instead of stranding the financial attempt.
- Record mutation-endpoint account throttles as the new
  `GatewayAccountRateLimitedBeforeSubmission` resolution code while retaining
  `GatewayProviderRateLimitedBeforeSubmission` for provider-wide readiness
  throttles. Both codes retain the renewal fast-to-slow retry curve, while
  their cross-subscription cooldown scopes remain account and provider.
  NMI HTTP 429 remains provider-scoped because it is a documented system-wide
  transport throttle with an indeterminate mutation outcome; only in-band code
  301 proves a not-submitted, merchant-account-scoped throttle. The account-mode
  query API collapses both signals into one `GatewayError::RateLimited`, so
  readiness failures conservatively retain provider-wide cooldown.
- Rename the public renewal rate-limit pacing symbols to match their expanded
  account-and-provider semantics:
  `RENEWAL_PROVIDER_RATE_LIMIT_FAST_RETRY_ATTEMPTS` to
  `RENEWAL_RATE_LIMIT_FAST_RETRY_ATTEMPTS`,
  `RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS` to
  `RENEWAL_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS`, and
  `provider_rate_limit_retry_after_seconds` to
  `rate_limit_retry_after_seconds`. Rename `RenewalAttemptState` fields
  `provider_rate_limited_attempt_count` and `last_provider_rate_limited_at` to
  `rate_limited_attempt_count` and `last_rate_limited_at`. Remove
  `RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS` in favor of
  `GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS`; 0.4.0 applies the same
  Cargo-breaking migration policy to the whole symbol family.
- Classify NMI HTTP 404 and 405 responses as terminal configuration defects,
  not transient `Unavailable` transport failures. The adapter maps a proven
  pre-transmission `Unavailable` to `GatewayNotSubmittedError::NotTransmitted`,
  which authorizes same-attempt, same-order replay.
- Normalize recovery and payment-method replacement reservation-time, plus
  enrollment admission-time, gateway mode changes to the service-level
  `GatewayConfigurationChanged` error used by the other payment flows, while
  their lower-level outcomes retain a distinct `GatewayAccountModeChanged`
  reason.
- Preserve that mode-change classification in concurrent enrollment and host
  reservation races. Typed verification and not-submitted errors retain both
  the required and observed modes for protected operator triage, while the
  high-level subscriber facade and durable `gateway_response_text` use
  provider-neutral, subscriber-safe wording. The durable text now says that the
  account mode did not match the deployment without revealing either mode.

## [0.3.0] - 2026-08-16

### Fixed

- Resume same-idempotency-key subscription recovery and payment-method
  replacement attempts that were durably prepared but not submitted, reusing
  their persisted attempt and gateway order identities.
- Expire abandoned unsubmitted renewal and recovery attempts locally, and
  exclude every never-submitted attempt from exact gateway reconciliation.
- Locally expire never-submitted initial attempts that version 0.2.0 exact
  reconciliation had already parked as `review_required`.
- Preserve exact gateway-readiness failure categories after reservation while
  keeping retry-safe prepared attempts pending on transient unavailability.
- Retire stale unsubmitted host charges together with their host-owned targets,
  and stop stale local attempts from blocking entitlement and deletion reads.
- Enforce durable account and provider cooldowns before host-charge gateway
  resolution, so a closed local gate performs no credential resolution or
  provider readiness I/O.
- Skip contended abandoned renewal/recovery rows during local cleanup while
  preserving them as semantic cancellation blockers instead of leaking a
  transient database lock failure.
- Include the normalized durable billing-contact snapshot in replay identity
  for every token-bearing foreground payment command. A same-key retry can
  refresh its memory-only token and candidate attempt ID, but changed contact
  now returns an idempotency conflict before provider submission.
- Preserve billing-contact first and last names separately in durable attempt
  identity, so distinct structured contacts cannot collide merely because they
  produce the same receipt display name.
- Reject structurally incoherent payment results, including host attempts in
  subscription results, mismatched applied subscriptions, and confirmation-
  pending results without an authoritative approved gateway outcome.

### Migration

- PostgreSQL schema v3 is now required. Stop all schema-v2 billing writers and
  apply `schema/v3/upgrade_from_v2.sql` transactionally before starting 0.3.
  Update host startup checks from `assert_runtime_schema_v2_compatible` to
  `assert_runtime_schema_v3_compatible` before restarting against schema v3.
  Historical combined attempt names become canonical first-name values with no
  last-name component; new attempts retain both normalized components. Do not
  restart a v2 writer after the cutover.

- Hosts upgrading an existing 0.2.0 deployment must extend each gateway
  account's reconciliation pass with the local payment-method replacement,
  subscription charge, enrollment, and (when configured) host-charge cleanup
  phases before claiming attempts for exact provider queries. These cleanup
  phases retire attempts that never crossed the provider boundary; run the
  bounded phases on every scheduled pass so a backlog drains safely. The
  enrollment phase automatically repairs affected 0.2.0 `review_required`
  rows; no schema migration or manual backfill is required.
- Subscriber and host-charge mutations may now return an exact
  `GatewayReadiness` error after committing a token-free attempt. Transient
  unavailability leaves it pending; determinate readiness failures persist a
  terminal result first. Hosts must not infer that `Err` means no durable
  attempt exists and must reuse the same idempotency key to recover or resume
  that canonical result.
- Replace `SubscriptionEnrollmentPaymentResult::new` and the optional
  subscription argument to `confirmation_pending` with the checked `applied`,
  `not_applied`, and `confirmation_pending` constructors. This prevents an
  approved result without an applied subscription and prevents non-applied or
  confirmation-pending results from carrying one.
- Update host-charge payment-result construction to return checked results,
  and derive confirmation-pending processor evidence only from an
  authoritative approved gateway outcome. Hosts that directly construct
  these public result values must handle the new validation errors.

## [0.2.0] - 2026-08-13

### Added

- Add explicit immediate-recurring and positive paid-trial offer terms with
  fixed-day or calendar-month billing periods, separate introductory and
  recurring prices, and durable versioned enrollment snapshots.
- Add `Entitlement::permits_product_access()` as the canonical subscription
  entitlement decision while retaining host authentication and authorization
  as host responsibilities.
- Add the redacted, request-scoped `SubscriptionPaymentContext` shared by
  enrollment, recovery, and payment-method replacement commands.
- Add checked `DunningRetryDelay` constructors for whole hours, whole days, and
  exact `std::time::Duration` values, plus array-friendly
  `DunningSchedule::from_delays` construction. Both iterator-based schedule
  constructors consume at most the supported sixteen steps plus one overflow
  witness.
- Add per-subscription relative dunning schedules, configurable exhausted
  behavior and past-due access, terminal `unpaid` state, and provider-neutral
  nonpayment lifecycle events.
- Add PostgreSQL schema v2 fresh-install, read-only v1 preflight and retry-
  reclassification audit, and transactional forward-only v1-to-v2 upgrade
  artifacts with fresh/upgrade catalog-parity coverage.
- Add high-level subscriber cancellation and discount claim/clear operations.
  They run exact end-user mutation admission; cancellation commits its
  canonical mutation and typed outbox event through one host-prepared
  transaction, while discount operations preserve their typed semantic
  outcomes without gateway or provider I/O.
- Add provider-neutral customer billing portal and exact-plan payment-history
  reads. Portal snapshots preserve canonical entitlement semantics and expose
  only a value-redacted, nonempty masked-card display; normalized absence is
  represented only by `None`. History uses a checked cursor page size and
  excludes provider references, transaction IDs, contacts, responses, and raw
  diagnostics.
- Add stable `due_renewals_page` dispatch pagination. Its cursor carries the
  first PostgreSQL-observed scan timestamp and strict scheduling key so hosts
  can drain more than one hundred unchanged due subscriptions without
  offset/timestamp-tie gaps or repeats. It freezes eligibility time rather than
  taking a cross-page snapshot, so concurrent mutable candidates can wait for
  a fresh scan; it is not a queue lease, and `due_renewals` remains the
  compatible fixed-first-page helper.
- Add default-feature `assert_runtime_schema_v2_compatible` startup validation.
  It uses the exact schema-contract catalog and fingerprint checks in one
  repeatable-read, read-only transaction, requires PostgreSQL 18, accepts
  separately named host-prefixed objects, and fails closed for v1, added
  canonical columns, or other canonical drift without embedding or exposing a
  production migrator. `SUPPORTED_POSTGRES_MAJOR_VERSION` exposes the required
  major to host startup code.
- Add non-exhaustive `SubscriptionBillingServiceError` dispositions for
  conflict, rejected, temporarily unavailable, misconfigured, and internal
  failures. Hosts can use the conservative retry helpers without exposing
  gateway diagnostics; retryability permits only resubmitting the same
  idempotent command and does not guarantee success.

### Fixed

- Keep runtime schema-v2 validation available during PostgreSQL concurrent
  reindexing by excluding only invalid `_ccnew` and `_ccold` shadow indexes
  whose locks and visible PostgreSQL progress both identify a non-initializing
  `REINDEX CONCURRENTLY`. Recheck that live evidence before commit and retry a
  transition once from a fresh snapshot; stale, cross-role-hidden, or
  independently created suffix-bearing indexes remain fail-closed.
- Make the dependency-advisory wrapper run without an empty-array expansion
  when no RustSec exception is needed, including on Bash versions before 4.4.
- Narrow the internal offer comparison helper to ignore only the recurring
  amount; currency and paid-trial pricing remain accepted terms.
- Enforce complete PostgreSQL schema artifacts from their first stable release
  tag, so shipped versions remain byte-immutable while the current unreleased
  version can still be corrected before publication.

### Changed

- Separate the economic `next_renewal_at` period anchor from the mutable
  `next_payment_attempt_at` scheduler clock. Only submitted determinate
  automatic-renewal failures consume dunning; operational and provider pacing
  remain independent.
- Remove renewal dispatch's forced full-candidate CTE materialization and make
  its ordered outer limit eligible for an early-stopping index plan. PostgreSQL
  plan choice remains cost-based, so hosts must rehearse representative data.
  Align both renewal and exact-plan payment-history keysets with explicit
  schema-v2 indexes. The runtime contract validates each complete index shape,
  including its table, access method, uniqueness, key ordering and null
  behavior, operator classes, included columns, predicate, and planner/write
  readiness.
- Replace raw protected-write transactions with a pool-created top-level
  `EntitlementWriteTransaction` that cannot commit before admission and an
  `AdmittedEntitlementWriteTransaction` returned only on success. Completed
  denials and SQL failures await a full rollback; cancellation remains
  fail-closed through ownership. Successful guards restore the caller's
  timeout and retain their locks through the host write.
- Make renewal, recovery, reconciliation, operator review, entitlement,
  cancellation, grants, deletion, and payment-method cleanup exhaustive over
  paid trials, scheduled dunning, and terminal unpaid history.
- Require `past_due` for fresh recovery reservations while honoring the exact
  `active` or `past_due` authority snapshotted by unresolved v1 attempts across
  the v2 maintenance cutover.
- Treat finite `LimitedMonths` discounts as recurring-calendar-month terms;
  paid trials consume zero discount periods, and incompatible recurring
  cadences are rejected through a dedicated `LimitedDiscountCadence` error
  before an attempt is created.
- Give subscriber-specific enrollment offer locks a typed reservation context
  with lifecycle stage, stable attempt identity, and idempotency identity.
  Attempt-history eligibility must exclude the supplied in-flight attempt.
- Mark `SubscriptionBillingServiceError` non-exhaustive. Consumers must retain
  a wildcard when matching its disposition, use `retry_after()` only as an
  exact delay when it is present, and rebuild/reconcile rather than retry a
  conflict as-is.
- Reduce provider card-brand text to the closed `PaymentCardBrand` vocabulary
  before customer display or host event projection. Unknown nonempty values
  become `Other`; exact provider evidence remains available only at its
  explicit persistence and reconciliation boundary. Recognize NMI's documented
  `diners` label as `DinersClub` and conformance-test its card-scheme vocabulary.
- Keep arbitrary host callback errors out of ordinary formatting and the
  standard error-source chain. Hosts that intentionally need the original
  value consume the wrapper with `into_source()`.

### Developer experience

- Replace wildcard crate-facade exports with explicit reviewed export lists
  while preserving the pre-release core and PostgreSQL root API inventories.
  Add a layered public-API guide and enforce rustdoc on the closed
  billing-event, high-level service, and host transaction boundaries. The
  small-project CI gate deliberately avoids a custom compiler-diagnostic debt
  baseline: it checks readable facades, rejects wildcard exports, builds
  warning-free docs, and runs doctests.
- Add a compile-tested host-owned version-1 billing-event envelope example.
  It retains the admitted event subject, maps every closed event variant,
  separates host kind/version and semantic-key columns from a minimized
  payload, and keeps Rust domain types out of the wire-format contract. Its
  replay value compares every stable field while excluding first-write
  identifiers/timestamps. A concrete same-transaction helper compares the
  untouched split columns and structural JSONB payload before exact typed
  reconstruction, so unknown or normalized fields cannot become an accepted
  replay. V1 owns every nested enum label instead of inheriting future core
  display changes. Its `Debug` output exposes only schema version and event
  kind, never subject identifiers or payload values.
- Package a focused README and explicit proprietary notice with every crate.
  Release preflight now rejects a package that omits either file.
- Verify all workspace targets on the declared Rust 1.88 minimum in CI and the
  release workflow. Add a RustSec gate whose sole exception is the unreachable
  RSA implementation locked behind SQLx's unused MySQL feature; the gate fails
  if that implementation ever enters a workspace build graph.

### Migration

- Schema-v1 hosts must prebuild the 0.2 application, stop all 0.1 billing
  writers, run `schema/v2/preflight_from_v1.sql`, apply
  `schema/v2/upgrade_from_v1.sql` transactionally, and roll forward with 0.2.
  Schema v1 remains byte-immutable and 0.1 writers must not restart after the
  v2 migration commits.
- Budget that maintenance window for the upgrade's transactional replacement
  of the renewal-dispatch index and construction of the exact-plan
  payment-history index. PostgreSQL scans the full payment-attempt heap while
  building that partial index and stores only non-host-charge entries, so the
  build can dominate large upgrades; rehearse against representative data
  rather than changing the atomic artifact to `CREATE INDEX CONCURRENTLY`.
- The upgrade intentionally reclassifies dunning history. Version 1's global
  five-attempt ceiling combined automatic-renewal and subscriber-recovery
  failures; version 2 counts only submitted determinate automatic renewals.
  Some `active` or `past_due` subscriptions that had stopped dispatching under
  v1 can therefore resume automatic collection after cutover. Run
  `schema/v2/audit_retry_reclassification_from_v1.sql` before maintenance and
  review the returned accounts.

### API migration from 0.1

- Construct one `SubscriptionPaymentContext` from the authorized subscriber
  payment request, then pass it with operation-specific data to
  `EnrollSubscription::new`, `RecoverSubscriptionPayment::new`, or
  `ReplaceSubscriptionPaymentMethod::new`. The context keeps ordinary debug
  output free of idempotency-key, payment-token, and billing-contact values.
- Rename `SubscriptionEnrollmentServiceError` to
  `SubscriptionBillingServiceError`. The service error covers enrollment,
  recovery, renewal, payment-method replacement, reconciliation, and optional
  host-charge orchestration. Its generic storage and application messages now
  use billing/payment terminology while retaining typed error sources.
- Standard error-chain traversal no longer reaches arbitrary application
  errors supplied by host transaction, event, charge-target, or operator-review
  callbacks. Classify a `SubscriptionBillingServiceError` first; only in an
  explicitly protected diagnostic path, destructure its owned callback-error
  variant and consume that wrapper with `into_source()`. Generic error
  reporters should retain the intentionally terminated source chain.
- Provider-free cancellation and discount transactions now surface pool
  acquisition timeouts and PostgreSQL serialization, deadlock, lock-timeout,
  and statement-timeout failures as `StorageTemporarilyUnavailable`. Its
  disposition is retryable without an invented delay. Other storage failures,
  including ambiguous provider-facing paths, remain internal.
- Renewal dispatch selection now applies non-attempt eligibility first and
  probes the indexed attempt history only for each candidate's exact current
  billing period. Eligibility, ordering, page size, and cursor semantics are
  unchanged; unrelated historical attempts are no longer globally aggregated
  for every page.
- Replace `next_monthly_billing_period` and `MonthlyBillingPeriodError` with
  `next_billing_period(start_at, rule)` and `BillingPeriodPolicyError`. Pass
  `SubscriptionPeriodRule::calendar_months(1)` for the former monthly behavior.
- Construct `SubscriptionOffer` with `RecurringSubscriptionTerms`,
  `SubscriptionStart`, and `RenewalFailurePolicy`. Replace
  `SubscriptionEnrollmentExpectedCharge` and `expected_charge()` with
  `SubscriptionEnrollmentExpectedTerms` and `expected_terms()`.
- Replace overrides of `SubscriptionOfferStore::lock_enrollment_offer` with the
  new `(connection, SubscriptionEnrollmentOfferContext)` signature. Branch on
  `context.stage()` only for stage-specific locking, and exclude
  `context.attempt_id()` from historical eligibility queries.
- After replacing the version-0.1 enrollment charge expectation with
  `SubscriptionEnrollmentExpectedTerms`, use `activation_projection()` when
  applying accepted terms. Its `initial_charge()` authorizes enrollment and
  `recurring_charge_after_initial()` is stored for the next recurring
  collection; these differ for an immediate start with a one-period discount.
- Use `PaymentAttemptFingerprint::for_subscription_initial_v2` for new
  enrollment terms. The `_v1` constructor and matcher exist only to recognize
  durable version-1 attempts during migration and reconciliation.
- Remove handling for `CancelSubscriptionOutcome::BlockedByPastDue`.
  Cancellation during dunning now returns `Canceled` after the existing
  in-flight renewal and payment-method-update fences pass.
- Treat cancellation outcomes as lifecycle outcomes: the newest exact canceled
  subscription returns `AlreadyCanceled` even after paid-through access expires,
  while a newest terminal `unpaid` subscription returns `NotFound`. Here,
  `NotFound` means there is no cancelable lifecycle; retained financial history
  may still exist.
- Discount-code administration now returns durable
  `SubscriptionDiscountCodeRecord` values. Listing and disabling codes no longer
  require a current offer or attempt to construct a quote; use
  `validate_subscription_discount_code` for current-offer eligibility and
  pricing. Active create/update operations still validate the locked offer.
- Replace customer uses of `MAX_RENEWAL_TERMINAL_ATTEMPTS_PER_PERIOD` and
  `RENEWAL_RETRY_AFTER_SECONDS` with the offer's `RenewalFailurePolicy`. Use
  `RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS` or
  `RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS` only for their named
  operational pacing paths. Prefer checked `DunningRetryDelay::hours`,
  `DunningRetryDelay::days`, or `DunningRetryDelay::try_from(Duration)` values
  with `DunningSchedule::from_delays`; the second-based APIs remain available
  for persistence adapters.
- Update event consumers for `SubscriptionStarted.phase`, the closed
  `SubscriptionPaymentFailureDisposition`, the accompanying
  `SubscriptionPaymentFailureAccess`, and the new `SubscriptionEnded` event.
  The failure event's `access` field is the canonical post-failure access fact;
  do not infer access from its disposition or current offer. `Subscription`
  now exposes phase, recurring-period, failure-policy, and scheduler facts.
  `Entitlement::PastDue` no longer inherently means that product access is
  suspended: use `Entitlement::permits_product_access()` for the canonical
  subscription decision. Inspect `PastDueAccess` separately only when the host
  needs to present the dunning reason. With
  `ContinueUntilDunningExhausted` plus `RemainPastDue`, the final failure's
  `access: Ended { .. }` is the revocation signal and no `SubscriptionEnded`
  event follows it.
- Treat `SchemaConformanceError` as non-exhaustive and handle
  `UnsupportedPostgresVersion` during startup. PostgreSQL 18 is the sole
  supported major; `SUPPORTED_POSTGRES_MAJOR_VERSION` is the machine-readable
  requirement.

## [0.1.1] - 2026-08-09

### Fixed

- Normalize PostgreSQL timestamps to database precision so round trips do not
  produce false optimistic-state mismatches.

### Changed

- Centralize locked payment-attempt terms and subscriber-readiness policy.
- Consolidate payment outcome, approved-application, and fallback processor
  evidence persistence while preserving the existing public constructors.

### Maintenance

- Restore clean-runner Jig bootstrap and enforce schema-backed SQLx metadata in
  repository policy CI.
- Add a release preflight and a manual crates.io trusted-publishing workflow
  that uses short-lived OIDC credentials.

## [0.1.0] - 2026-08-08

- Initial crates.io release of `syrup-rail`, `syrup-rail-postgres`,
  `syrup-rail-nmi`, and `syrup-rail-nmi-client`.

[Unreleased]: https://github.com/bpcakes/syrup-rail/compare/v0.5.2...HEAD
[0.5.2]: https://github.com/bpcakes/syrup-rail/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/bpcakes/syrup-rail/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/bpcakes/syrup-rail/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/bpcakes/syrup-rail/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/bpcakes/syrup-rail/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bpcakes/syrup-rail/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/bpcakes/syrup-rail/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bpcakes/syrup-rail/tree/v0.1.0
