# Merge v0.5.1 into the 0.6.0 integration line

Integrate the stable `v0.5.1` release from `origin/master` into
`integration/0.6.0` without regressing the 0.6.0 schema-v5 contract or its
newer payment and locking behavior. The payment-evidence resolutions follow
[`docs/security/threat-model.md`](../../docs/security/threat-model.md): provider
identifiers and decisions remain untrusted, indeterminate mutations are never
blindly retried, and conflicting evidence must remain available for exact
reconciliation or operator review.

## Progress

- [x] Refresh remote refs and point `origin/HEAD` at `origin/master`.
- [x] Preserve the existing local Jig receipts and fast-forward
  `integration/0.6.0` to `origin/integration/0.6.0`.
- [x] Merge `origin/master` at `v0.5.1` and resolve the release-line conflicts.
- [x] Preserve schema v5, version 0.6.0, and the integration-only concurrency
  and cooldown coverage.
- [x] Carry the exact coordinated internal dependency requirements forward as
  `=0.6.0`.
- [x] Update repository-owned default-branch, CI, and release references from
  `main` to `master`.
- [x] Confirm `cargo check --workspace --all-targets --all-features --locked`.
- [x] Run the required Jig contract, formatting, Clippy, SQLx, and workspace
  test gates.
- [x] Run release metadata, immutable-schema, public-API, and changed-file Rust
  LOC checks.
- [x] Review the final diff and close the work session.

## Surprises & Discoveries

- The remote still temporarily exposes both `main` and `master` at the same
  commit, but its symbolic `HEAD` is `master`; repository-owned automation must
  therefore use `master` now.
- The 0.5.1 branch split several large Rust test and support modules after the
  0.6.0 line had independently consolidated reconciliation dispatch. The merge
  retains the newer dispatcher while routing payment-method replacement into
  the stricter 0.5.1 reconciliation implementation.
- The pre-existing local receipt edit and this work session's metadata applied
  cleanly after the merge as unstaged append-only state.
- Retaining both branches' application tests in one file exceeded the absolute
  Rust source-size gate. Adopting the upstream split and extracting eight
  integration-only helpers/tests into `application/integration_regressions.rs`
  preserved both suites and brought the changed file under the limit.

## Decision Log

- Keep the workspace and crate version at 0.6.0 and preserve schema v5; taking
  the 0.5.1 version or schema-v4 documentation would regress the integration
  line.
- Preserve the complete 0.5.1 changelog entry as release history beneath the
  0.6.0 entry.
- Use the 0.5.1 payment-method replacement reconciliation implementation for
  identity-conflict and approved-evidence retention, while keeping the 0.6.0
  generic dispatcher for operation selection and its integration-only tests.
- Create a normal two-parent merge commit so `origin/master` ancestry is
  explicit; do not push without a separate user request.

## Outcomes & Retrospective

The completed two-parent merge commit is
`2b8b51b385b91fc96a8ddcbba493e00ce6823b4a`. It has
`1da25bbfe5e1b89b89e80ff9e3b24f959f54efbf` and the `v0.5.1` commit
`1650257918f617d5b8634c40c370e7fb778b06e6` as parents. The repository contract,
workspace tests, SQLx metadata, formatting, Clippy, release metadata,
immutable-schema comparison, public-API documentation, and changed-file Rust
LOC checks all pass. No schema artifacts changed, and no conflict markers
remain. The branch is intentionally not pushed; only append-only Jig state and
the pre-existing receipt edit remain outside the merge commit.

## Context and orientation

The integration branch already contained v0.5.0 plus 0.6.0-only schema-v5,
domain-model, and concurrency changes. The stable branch added v0.5.1 payment
certainty, diagnostic provenance, reconciliation, and release-hardening work.
The main overlap is under `crates/syrup-rail-nmi-client`,
`crates/syrup-rail-nmi`, `crates/syrup-rail`, and
`crates/syrup-rail-postgres/src/enrollment_application`.

## Plan of work

Use Git's common ancestor to merge all non-overlapping 0.5.1 changes. Resolve
version and schema documentation in favor of 0.6.0. Resolve behavioral overlap
by preserving 0.5.1's fail-closed payment evidence rules in the newer 0.6.0
module structure. Update renamed-default-branch references and validate all
configured repository gates.

## Concrete steps

1. Fetch `origin/master`, tags, and `origin/integration/0.6.0`.
2. Temporarily stash only local append-only Jig state, fast-forward integration,
   and restore it.
3. Open a Jig work session, stash its metadata, and merge `origin/master` with
   an explicit merge commit.
4. Resolve conflicts, run a workspace all-target compile, commit the merge, and
   restore Jig state.
5. Run `scripts/jig work check`, `scripts/jig check fmt`,
   `scripts/jig check clippy`, `scripts/jig check sqlx`, and
   `scripts/jig check test` as required.

## Validation and acceptance

Acceptance requires no conflict markers, a two-parent merge commit containing
`origin/master`, schema v5 and version 0.6.0 still current, all repository-owned
default-branch references naming `master`, and every configured Jig gate
passing. The final worktree may contain only the pre-existing receipt append and
new append-only Jig work records.

## Idempotence and recovery

The initial receipt stash was restored and dropped. The merge-work stash was
restored and dropped after the merge commit. If validation exposes a semantic
problem, amend the integration result with a follow-up commit; do not rewrite
the released `v0.5.1` tag, shipped schema artifacts, or append-only state.

## Interfaces and dependencies

No new external dependency is introduced. The four internal crate dependencies
remain path dependencies pinned to the exact coordinated workspace version.
The public diagnostic and payment-result APIs added by v0.5.1 are carried into
0.6.0, including `observation_diagnostics()` and typed anomaly diagnostics.
