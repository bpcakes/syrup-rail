# Goal Harness

## Objective

Integrate the published release/0.4.0 line into main without rewriting shipped schema v4, promote main's unreleased schema work to v5, and prepare the combined workspace as 0.5.0.

## Verifiable Stopping Condition

The combined history contains v0.4.0; schema v4 matches the published tag byte-for-byte; schema v5 combines gateway-mode and reversal-attestation contracts with a forward v4 cutover; Rust APIs, persistence, docs, metadata, and release version agree on 0.5.0; all required Jig gates pass.

## Validation Loop

- scripts/jig check contract
- scripts/jig check test
- scripts/jig check sqlx
- scripts/jig check fmt
- scripts/jig check clippy

## Constraints

- Do not modify shipped schema/v1 through schema/v4 artifacts from their published versions.
- Preserve the threat-model boundaries and explicit transaction semantics.

## Checkpoints

- [ ] Merge release/0.4.0 and resolve append-only metadata.
- [ ] Establish schema v5 and runtime contract.
- [ ] Integrate application code and pass focused checks.
- [ ] Pass all Jig gates and release review.

## Configured Jig Gates

- contract: check (jig.contract_check)
- tests: check (jig.test)
- sqlx: check (jig.sqlx_check)

## Progress Log

- Goal harness created. Keep this section short and append dated checkpoints, failed attempts, and validation evidence.

## Notes

No extra notes.
