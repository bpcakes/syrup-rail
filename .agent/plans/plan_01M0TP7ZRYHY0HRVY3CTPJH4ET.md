## Purpose
Resolve the comprehensive-review findings without changing the supported atomic cutover contract.

## Scope
- Restore the schema-v3 runtime-validation release note.
- Explain the atomic cutover as a rollout-policy tradeoff, acknowledging the lower-lock staged alternative.
- Add v4 rejection tests for untouched v3 and canonical drift.
- Exhaustively prove the six allowed and two forbidden tuple combinations across preflight and v4 constraint behavior.
- Remove the cluster-scoped validator role on every test result path.

## Safety
Do not edit shipped schema/v1 through schema/v3 artifacts. Do not add automated financial-evidence repair or a second migration protocol.

## Verification
Run focused v4 tests, schema immutability, formatting, Clippy, full tests, SQLx, and contract gates.