# Dependency advisory policy

Every release and relevant CI run executes `scripts/check-advisories.sh`. A
dedicated workflow also runs it every Monday so a newly published advisory is
detected without waiting for a repository change. The script audits the
complete lockfile and denies vulnerabilities, warnings, unmaintained packages,
unsound packages, and yanked releases. The SQLx 0.9.0 lockfile no longer
contains `rsa`. The current exceptions are the temporary test/tool-only
maintenance exception and the unreachable SQLite driver exception below.

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

## Unreachable SQLite driver exception

`RUSTSEC-2026-0459` reports that `libsqlite3-sys` 0.6.0 through 0.38.2 bundle
a SQLite release affected by CVE-2026-119375. A process is exposed only when
it opens, through the bundled SQLite, a database file and `-wal` that a less
trusted party could have written. The fix is `libsqlite3-sys` 0.39.0. See the
[RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0459).

The lockfile records `libsqlite3-sys 0.30.1` through SQLx's optional SQLite
driver (`sqlx-sqlite 0.9.0`), which requires `libsqlite3-sys >=0.30.1, <0.38.0`,
so no patched version can be locked with SQLx 0.9.0. No workspace member
enables that driver: the workspace selects SQLx with default features disabled
and only the `runtime-tokio`, `postgres`, `macros`, `derive`, `uuid`, `json`,
and `chrono` features. `cargo tree --locked --workspace --all-features --target
all -e all -i libsqlite3-sys` finds no path from any library, test, example,
or tool, including build and dev edges. The crate is therefore never compiled
or linked, and no Syrup Rail build opens a SQLite database.

The audit script ignores only this advisory ID, and only while the locked
version is exactly `0.30.1` and the complete workspace graph across all
features and targets does not reach it. A reachable or changed
`libsqlite3-sys` fails the check, as does every other warning or
vulnerability. Maintainers must remove the exception when an SQLx upgrade
locks a patched `libsqlite3-sys` or drops it from the lockfile.
