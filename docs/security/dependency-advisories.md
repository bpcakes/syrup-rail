# Dependency advisory policy

Every release and relevant CI run executes `scripts/check-advisories.sh`. A
dedicated workflow also runs it every Monday so a newly published advisory is
detected without waiting for a repository change. The script audits the
complete lockfile and denies vulnerabilities, warnings, unmaintained packages,
unsound packages, and yanked releases. The SQLx 0.9.0 lockfile no longer
contains `rsa`. The only current exception is the temporary test/tool-only
maintenance exception below.

The script retains its reachability guard for historical SQLx lockfiles: any
reachable `rsa` dependency fails the check. New exceptions require a written
reachability and impact analysis.

## Temporary test/tool maintenance exception

`RUSTSEC-2026-0440` reports that every version of `astral-tokio-tar` is
unmaintained as of November 3, 2026; it does not identify a vulnerability or a
patched release. See the [RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0440).
Testcontainers still uses the crate; its replacement is tracked in
[upstream PR #1006](https://github.com/testcontainers/testcontainers-rs/pull/1006).

The current locked path is `postgres-test-harness` → `testcontainers 0.27.3`
→ `astral-tokio-tar 0.6.4`. The harness is a dev-dependency of the PostgreSQL
crate and a dependency of the SQLx verification tool. It is absent from the
normal/build graph of all library workspace members, across all targets and
features. Testcontainers contains archive-copy/build helpers, so this exception
does not claim the crate is unreachable from tooling or that unmaintained
archive handling is safe for hostile inputs. The test/tool environment remains
exposed to dependency maintenance risk; deployed billing libraries do not gain
this dependency.

This exception permits the maintenance warning until December 5, 2026
(exclusive), to keep integration tests and schema verification running
while upstream replaces it. The audit script fails if that date is reached,
the locked version changes, or the dependency enters any library's runtime or
build graph. It ignores only this advisory ID, retaining denial of all other
warnings and vulnerabilities. Maintainers must remove the exception when a
compatible harness/Testcontainers update drops the crate, or explicitly
reassess it before expiry. Upgrading to current Testcontainers 0.28 alone does
not remove the unmaintained dependency.
