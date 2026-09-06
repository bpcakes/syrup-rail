# Reconcile the complexity refactor onto the v0.5.2 integration base

Rebase the current uncommitted lint configuration, behavior-preserving Rust
refactors, and host-charge transaction finalization onto
`origin/integration/0.6.0` at `fe8cdf8`. Preserve all v0.5.1 and v0.5.2
payment-evidence, identity-conflict, source-layout, and lifecycle-alert
behavior.

## Progress

- [x] Fetch and audit the current upstream branch.
- [x] Trial-apply the tracked patch in a temporary worktree and identify nine
  textual conflicts.
- [x] Preserve the dirty state, fast-forward the local branch, and reapply it.
- [x] Resolve conflicts using the upstream module layout and behavior as the
  baseline.
- [x] Refactor every Rust 1.88 cognitive-complexity offender at threshold 20,
  including new upstream offenders.
- [x] Run focused tests and all required repository gates. Focused core,
  NMI-client, host-charge, and enrollment-application tests are green, as are
  formatting, Rust 1.88 Clippy, locked workspace tests, SQLx, contract, public
  API, advisory, package-release, and changed-file policy checks.
- [x] Review the final diff and close the work item.

## Surprises & Discoveries

- Upstream is twelve commits ahead and includes the v0.5.1 and v0.5.2 release
  merges.
- A three-way trial application applies 23 tracked files cleanly and conflicts
  in nine files where upstream reorganized or hardened payment workflows.
- Rust 1.88 Clippy reports sixteen offenders on the upstream source tree. Most
  correspond to the current refactor, while `outcome_support.rs` and a new
  host-charge conflict path require fresh upstream-aware refactoring.
- Upstream split host-charge tests into owned submodules. The rollback
  regressions now live in `host_charge_application/tests/rollback.rs` rather
  than rebuilding the former monolithic inline test module.
- The default parallel `jig.test` invocation caused the shared PostgreSQL test
  setup to collapse under concurrent host/container load (48 database-free
  tests passed and 243 database-backed tests failed together). The identical
  workspace/all-features suite passed both the locked gate and the plan-linked
  `jig.test` gate with `RUST_TEST_THREADS=1`; this is an execution-environment
  limitation, not a source regression.

## Decision Log

- Treat upstream payment-evidence provenance and identity-conflict behavior as
  authoritative. Port structure and transaction safety around it rather than
  accepting the old side of conflicts wholesale.
- Keep a named Git stash after successful reconciliation so the original dirty
  state remains recoverable until the user decides to remove it.
- Keep transaction ownership at the outer orchestration boundary: borrowed
  connection helpers return semantic outcomes; the owner explicitly commits or
  rolls back.
- Pin the cognitive-complexity evaluator to Rust 1.88 because the configured
  threshold is heuristic and the release gate denies warnings.

## Outcomes & Retrospective

The local `integration/0.6.0` branch now exactly matches upstream commit
`fe8cdf8` before the reconciled working changes. Twenty-three tracked paths
applied directly; nine conflicts were rebuilt against the upstream layout so
the v0.5.1/v0.5.2 payment-evidence and identity-conflict behavior remained
authoritative. Obsolete NMI-adapter and top-level enrollment refactors were
dropped because upstream had already brought those functions below the lint
threshold.

All sixteen Rust 1.88 cognitive-complexity offenders were resolved without
lint suppressions. The host-charge path now models transaction outcomes with
private semantic types and centralizes commit/rollback finalization, including
explicit rollback regressions for ownership conflict and speculative
additional-charge paths. This narrows the bug surface compared with scattering
transaction completion across nested branches.

Verification passed, including the 1,498.7-second locked workspace test gate
(`receipt_01M1RZW8KFEAYYZAM41YYZTKRD`) and the plan-linked contract/test/SQLx
batch (`receipt_01M1S1BWXE22622SSRRVFNBG0Q`). The original dirty patch remains
recoverable as `stash@{0}`. No commit or push was performed.

## Context and orientation

The local branch begins at `1da25bb` and has an uncommitted refactor that
enables `clippy::cognitive_complexity` at threshold 20 across the workspace.
Upstream `fe8cdf8` adds the stable v0.5.1/v0.5.2 bases and substantially
reorganizes NMI, enrollment-application, and host-charge code. The current
PostgreSQL module ownership rules are in
`crates/syrup-rail-postgres/AGENTS.md`.

## Plan of work

1. Stash all tracked and untracked changes under a unique descriptive name.
2. Fast-forward `integration/0.6.0` to `origin/integration/0.6.0` and apply,
   without dropping, that stash.
3. Resolve each conflict by starting from upstream behavior and porting only
   still-relevant refactoring or transaction-safety changes.
4. Run Rust 1.88 Clippy with the configured threshold, refactor any remaining
   offenders, and avoid lint suppressions.
5. Run focused tests for changed payment workflows, then Jig formatting,
   Clippy, locked tests, SQLx, contract, public API, and release preflight.

## Concrete steps

Use `git stash push --include-untracked`, `git merge --ff-only`, and
`git stash apply`. Inspect conflicts with `git diff --diff-filter=U`, edit with
small patches, and stage only to record conflict resolution. Once the index is
conflict-free, unstage the result so the final worktree remains an ordinary
uncommitted patch unless the user asks for a commit.

## Validation and acceptance

Success means `origin/integration/0.6.0` is the local ancestor/base, no
unmerged entries remain, Rust 1.88 Clippy reports no function above threshold
20, focused payment and rollback regressions pass, and the required Jig gates
either pass or have a concrete independently reproduced infrastructure failure.

## Idempotence and recovery

The reconciliation stash is applied rather than popped. If resolution fails,
the local branch can be restored from that stash because the original entry is
retained. Temporary audit worktrees are removed after use. No commit, push,
tracker mutation, or destructive cleanup is authorized by this plan.

## Interfaces and dependencies

No public API change is intended. `BillingTransaction` gains documentation of
its existing rollback-on-drop cancellation requirement; private semantic
outcome types centralize transaction finalization. Cargo and CI adopt the
workspace lint configuration and Rust 1.88 Clippy evaluator.
