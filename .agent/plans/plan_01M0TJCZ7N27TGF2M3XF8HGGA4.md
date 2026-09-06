## Progress

- [x] Add forward-only schema v4 artifacts and strict tuple constraint.
- [x] Make v4 catalog validation the bounded runtime path while retaining v3 compatibility.
- [x] Add migration, conformance, and regression tests.
- [x] Update release and integration documentation.
- [x] Run SQLx, formatting, lint, and test gates.

## Surprises & Discoveries

- No repository cardinality ceiling or startup latency budget exists for retained attestations.
- Closed R-12 explicitly deferred the database pair constraint to an optional forward-only v4.
- PostgreSQL validates a new CHECK constraint against existing rows; a validated immutable row constraint can then be checked through the catalog without rereading table data at startup.
- The shipped-artifact gate includes versioned README files, so all v3 bytes remain unchanged and current guidance lives outside `schema/v3/**`.

## Decision Log

- Preserve shipped v3 bytes; add v4 install and v3-to-v4 upgrade artifacts.
- Validate legacy rows once while adding the v4 constraint, then rely on the validated catalog contract at startup.
- Retain `assert_runtime_schema_v3_compatible` for staged upgrades; make `assert_runtime_schema_v4_compatible` the documented current startup API.
- The existing threat model remains unchanged: this moves validation of retained financial evidence into the host-owned migration boundary and creates no new authentication, authorization, credential, or operator boundary.

## Outcomes & Retrospective

Schema v4 now encodes the complete typed external-reversal tuple matrix. Fresh
and upgraded catalogs have fingerprint `0x023d017191bedc34`; incompatible v3
rows abort the transactional cutover, compatible rows survive, and the v4
startup assertion succeeds for a role with no SELECT privilege on the retained
attestation table. Contract, full workspace test, SQLx, formatting, Clippy, and
shipped-schema immutability checks pass. One initial full-test attempt hit a
test-container port-exposure failure before its test body; the exact test and
the complete gate both passed on rerun.

## Context and orientation

Schema v3 permits two external-reversal prior/final combinations rejected by the typed runtime. The v3 startup checker compensates with an unbounded live-table query. Schema v4 encodes the typed matrix in a validated CHECK constraint.

## Plan of work

Create complete v4 install and forward-only upgrade artifacts, expose v4 contract constants and validation, limit live-row preflight to legacy v3, extend test fixtures and v4 tests, then update host cutover documentation.

## Validation and acceptance

Fresh v4 and upgraded v3 catalogs have equal fingerprints; incompatible v3 rows fail upgrade; compatible rows survive; v4 startup validation succeeds without SELECT privilege on the attestation table; repository gates pass.

## Idempotence and recovery

The upgrade is host-transactional. A constraint-validation failure rolls back, allowing operators to remediate v3 data and retry before starting v4 writers.
