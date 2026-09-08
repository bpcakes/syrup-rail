# Bound shared-method eligibility and tolerate lifecycle-only reconciliation

Full-branch round 2 reviewed 9f30107eaf13f4a84fa4a1aaa04299246fedcc09 through
6e4c85ade64390127e0343d49e774b3e231a357f. Claude and Codex completed with matching
complete fingerprint 47c8fc2b94e7d8efb2e689b50bde8e933aac44c62f6ac514ecb6348c80a91ff0.

## Progress

- [x] Research and adjudicate the frozen review reports.
- [x] Reproduce the account-wide scan with exact read and locked production SQL.
- [x] Add query-plan and lifecycle-overlap regressions, establishing negative controls.
- [x] Use method-linked approval evidence to find current references through existing indexes.
- [x] Remove the attempt-only timestamp from the candidate snapshot and clarify operational docs.
- [x] Pass focused and full required gates before committing this implementation checkpoint.

After the correction commit, run full-branch round 3 of the user's maximum 3.

## Decisions and evidence

Codex identified missing selective index support for the unrestricted current
subscription EXISTS. The owner index is partial on active/past_due; filtering
those statuses would change eligibility for historical subscriptions. PostgreSQL
18 on the canonical v4 schema with 4,096 subscribers in one account confirmed
that BOTH exact production statements use a sequential current_subscription
scan, discard 4,095 unrelated rows and touch 157 buffers. Generic plans agree.
Use the existing payment-method attempt index to find approved provenance for
referencing subscriptions, then probe those subscription IDs. Keep all ownership
checks and global latest-approval supersession. Do not change shipped schema v4.
Add a representative plan regression using the exact production statements.

Claude identified avoidable rejection when lifecycle reconciliation alone bumps
attempt.updated_at. The reconciliation writer changes lifecycle observations,
not approved identity or the payment-method/subscription projection. Remove that
one timestamp from Candidate and its SQL projection; keep method/subscription
timestamps for away-and-back races, and recheck every approved identity predicate.
Exercise an actual lifecycle reconciliation while the query is blocked. Refresh
must preserve the resulting financial rows rather than revert their observation.

Retain the gateway-account share lock: it closes the configuration-rotation
write window. Clarify that its brief critical section can also block account
configuration/cooldown writes, so host backfills must stay below financial traffic.

Retain the established provider cooldown and host-owned admission boundary.
NMI explicitly documents system-wide HTTP 429 spanning Payment and Query APIs
(https://docs.nmi.com/reference/rate-limiting). The coarse error cannot safely
establish an independent quota. The public typed result identifies refresh
throttling to the caller; adding a mutation-admission dependency to a standalone
read does not enforce host endpoint concurrency. No speculative query-only
cooldown or policy bypass is introduced.

Ineligible intentionally hides wrong identity as documented. Payment type is
outside this operation's explicit brand/last-four/expiry display contract.
Canonical financial evidence remains immutable: richer newly parsed metadata is
not an exact replay of an older normalized ProcessorEvidence. Document that
upgrades/retries retain that normalized evidence and use this independent
refresh for display enrichment; do not loosen financial replay equality.
Client/adapter and PostgreSQL tests cover their respective boundaries. A captured
Classic sale response and end-to-end host integration remain external validation,
not evidence supplied by synthetic fixtures. No new dependencies are needed.

## Validation

Run negative controls for the new plan and lifecycle tests, then focused metadata
tests, fmt, Clippy, public API, contract, SQLx and the full serialized workspace
suite. Preserve schemas, manifests and append-only workflow records. Exclude the
mixed Beads export from commits. Commit after fresh gates and then review the
clean full branch once more.

## Results so far

The exact production read and locked queries reproduced the scan. The new plan
regression rejected the old SQL. The actual lifecycle-overlap test rejected the
old attempt timestamp fence with ChangedDuringQuery instead of Updated; its
initial overlong harness project name was corrected before this negative control.
All 25 behavioral metadata tests pass after the fixes. The plan test additionally
caught redundant approval-owner filters choosing the subscriber-wide attempt
index. Those filters duplicate the canonical composite method-owner foreign key;
removing them retains method-index lookup while the outer candidate and current
subscription still explicitly check ownership. Both production statements are
shared constants, so the plan test explains their exact text, including row locks.
The fixture also checks active, past_due, canceled and unpaid references.

Formatting, Clippy and public API checks passed before that final SQL refinement;
required full checks will bind the final source snapshot. Schema v4 is unchanged.

The final exact-statement generic-plan regression now passes for both queries,
including executable checks of all four subscription statuses. Full gates are
running on that final SQL and source snapshot.

## Verified implementation checkpoint

All final gates passed: 110 core, 24 NMI adapter, 224 NMI client and 266 PostgreSQL
unit tests (624 total), four example tests and four doctests. The PostgreSQL suite
completed in 701.87 seconds. Contract, SQLx, formatting, Clippy and public API
checks passed. Jig reported all required gates fresh before staging.

Batch receipt: receipt_01M1YBWN1H2RDMAJFWE1918NTA. Individual receipts:
contract receipt_01M1YB43XQDTXD79VDQFRGWV70; SQLx
receipt_01M1YB5M35GG629T7ZRJXKZE2C; tests
receipt_01M1YBWMZZE7Z0QCZM3S6SXM9Y. Final Clippy receipt:
receipt_01M1YB3F2N2SVWBWJ1TR8113XN.

The isolated PostgreSQL plan-reproduction server was stopped and removed. The
completed review checkouts were removed after scope verification. This closes the
implementation checkpoint only; commit and full-branch round 3 follow.
