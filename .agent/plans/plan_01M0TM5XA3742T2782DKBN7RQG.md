## Purpose
Resolve the post-review release communication, migration-operability, and test-coverage findings for schema v4 without weakening financial-evidence constraints or changing the atomic cutover contract.

## Scope
- Set the unreleased breaking line to 0.4.0 consistently across workspace metadata and package documentation.
- Separate breaking API changes and v4 host migration instructions in CHANGELOG.md.
- Add a read-only v3-to-v4 preflight that reports retained-row volume and incompatible tuple count.
- Document ACCESS EXCLUSIVE scan behavior, representative-data rehearsal, and maintenance-window ownership.
- Prove refund and void forbidden tuples each abort the migration before audited fixture remediation.

## Safety
Preserve immutable schema/v1 through schema/v3 bytes. Keep v4 as one host-owned transaction with stopped writers. Never auto-rewrite retained financial evidence.

## Verification
Run focused schema-contract tests, format, clippy, SQLx, test, contract, and inspect schema immutability and the final diff.