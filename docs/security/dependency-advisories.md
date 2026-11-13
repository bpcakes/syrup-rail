# Dependency advisory policy

Every release and relevant CI run executes `scripts/check-advisories.sh`. A
dedicated workflow also runs it every Monday so a newly published advisory is
detected without waiting for a repository change. The script audits the
complete lockfile and denies vulnerabilities, warnings, unmaintained packages,
unsound packages, and yanked releases, except for the explicitly guarded
lockfile-only advisory below. The test harness uses its supported external-server
mode, removing Testcontainers, unmaintained `astral-tokio-tar`
([RUSTSEC-2026-0440](https://rustsec.org/advisories/RUSTSEC-2026-0440.html)), and
`rustix` ([RUSTSEC-2026-0486](https://rustsec.org/advisories/RUSTSEC-2026-0486.html))
from both the resolved graph and lockfile. Neither advisory has an exception.
`scripts/with-test-postgres.py` independently provisions the disposable server
for local and CI gates, while the harness retains database-lease ownership.

The script retains its reachability guard for historical SQLx lockfiles: any
reachable `rsa` dependency fails the check. New exceptions require a written
reachability and impact analysis.

## Unreachable SQLite lockfile entry

[RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459.html) concerns
`libsqlite3-sys` versions bundling SQLite before 3.54.0: a crafted database and
WAL file pair can expose uninitialized memory when opened. The patched crate
line starts at 0.39.0. SQLx 0.9.0's optional SQLite adapter requires
`libsqlite3-sys >=0.30.1, <0.38.0`, so a compatible lockfile-only upgrade cannot
select the patch. Syrup Rail uses PostgreSQL; its complete workspace graph
contains neither `sqlx-sqlite` nor `libsqlite3-sys`, despite Cargo.lock retaining
the optional dependency. No compiled package here links that SQLite code.

The wrapper checks this claim on every invocation with:

```sh
cargo tree --locked --workspace --all-features --target all -e all --prefix none --format '{p}'
```

This includes every workspace member, target, feature, and normal/build/dev edge.
Only when the sole locked `libsqlite3-sys` version is `0.30.1` and **every
version** is absent from this graph may the wrapper pass
`--ignore RUSTSEC-2026-0459`. A changed locked version or a reachable version,
including a future patched version, fails closed; graph-resolution errors stop
before audit. Package matching is exact. The historical `rsa` guard follows the
same rule for `RUSTSEC-2023-0071`. All other advisories and warnings remain denied,
and the audit exit status is preserved. Wrapper tests cover both exceptions,
other versions and package names, graph failure, and unrelated audit failure.

Remove the SQLite exception when the optional lockfile entry disappears or SQLx
permits a patched SQLite version, and before intentionally enabling SQLite. Any
new reachable SQLite use needs its own patched dependency review; this exception
does not assert safety for downstream workspaces with different feature graphs.
