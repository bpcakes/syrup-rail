Resolve release fixture exact-pin drift, default-feature documentation coverage, and SQLx current-schema selection. Preserve immutable schema artifacts and existing worktree edits.

Root-cause research:
- Exact dependency pins landed separately from the release-wrapper fixtures: an integration omission. Positive and negative wrapper cases now enforce the contract.
- All-feature documentation checks hid missing default-feature symbols. Both public API configurations are now checked.
- Integration fixtures and SQLx selected the current schema independently. A dependency-free shared selector now owns the choice, with fresh-install runtime conformance coverage.
- Opus identified unreachable payment-method replacement policies in shared approval parking. The shared reservation type now accepts only the three subscription workflows that actually use it; replacement retains its existing separate path.
- Public lifecycle accounting and schema-v4 helper documentation lost important qualifications. Those contracts are explicit again. Legacy pending-evidence catalog objects remain for versioned compatibility.

Verification:
- Required contract, workspace test, and SQLx gates passed on the final source snapshot; batch receipt receipt_01M1RHGBA43GWRYWR6KBDJPYRH.
- Default/all-feature documentation and doctests, release-wrapper tests, formatting, and Clippy passed.
- Backend tests used a disposable PostgreSQL 18 server through POSTGRES_TEST_ADMIN_URL after per-container startup flakes in the earlier run.
- Shipped schema v1-v4 artifacts are unchanged.
- Native Codex and repeated whole-branch Claude Opus reviews completed against v0.5.2, including final source snapshot 679d0b0135a870b61d34993cef7c2be899758eff. No actionable merged findings remain.

Final Opus report adjudication:
- Renewal readiness/Noop: intentional behavior, explicitly covered for all five errors in enrollment_application/tests/foreground/readiness.rs. resolution.rs classifies these codes for bounded infrastructure retries and cooldowns; enrollment_application/renewal.rs excludes them from customer dunning. Returning Payment instead would be a policy/API behavior change, not correction of an established contract violation.
- Reversal expect: both callers guarantee matching attempt kinds through locator validation or persistence of that same attempt. No reachable failing input was identified; no speculative defensive path added.
- Lifecycle counter simplification: current guards enforce newly staged counts and integration tests cover replay accounting. The proposal is an optional refactor, not a demonstrated defect.
- Scrub lock discovery: deletion.rs explicitly requires the host to stabilize creation of new subscriber billing rows. The proposed race violates that existing caller contract.
- Shell positional arguments: all original arguments have already been parsed, and subsequent code intentionally uses the replacement package arguments. The suggested failure requires hypothetical future code.
- Method placement and SQLx crate-root validation: stylistic or hypothetical. The Cargo command already fails on an invalid working directory; include_str tracks schema input changes.

Review limits: Claude performed static review because its sandbox denied Cargo commands; local build/test evidence above supplies runtime validation. The vendored Jig installer was checked at its pinned-source/checksum boundary, not audited line by line. No live NMI requests or production host migration rehearsal were performed.
