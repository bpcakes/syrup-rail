# Releasing Syrup Rail

Syrup Rail releases all four publishable crates at the same version. Publish
them in dependency order:

1. `syrup-rail`
2. `syrup-rail-nmi-client`
3. `syrup-rail-nmi`
4. `syrup-rail-postgres`

The Postgres package has a versioned development dependency on the NMI adapter,
so the adapter must also be available before packaging Postgres.

The workspace's internal dependency requirements must exactly match the
release version. The four crates share payment-evidence semantics as well as
Rust APIs, so an apparently compatible patch-level mix can change conservative
diagnostic routing. In particular, the Postgres and NMI packages can use APIs
and policy added in the matching core release and must not claim compatibility
with an older or newer core package.

## Branches

- `master` is the default branch and prepares the 0.6.0 SQLx upgrade.
- `dev/0.6.0` contains separate unfinished development; its historical branch
  name no longer reserves the 0.6.0 release number.
- `main` and `integration/0.6.0` are historical references; target new work at
  one of the two active branches above.

Fix stable issues on `master` and carry applicable fixes forward into
`dev/0.6.0`. Both active branches run push CI. Publishing is restricted to
`master`; developing on the default branch does not publish a release.

Version 0.6.0 upgrades SQLx to 0.9.0 and Rust to 1.94, starting from the
IdentityPro-consumed `3a7756d1bcb6f7e1c8812b4ea0c46a210ebb413a` behavior.
The public SQLx pool, connection, transaction, and error types make this a
breaking package release. It preserves schema v6 byte-for-byte; hosts already
on v6 need no database migration for this upgrade.

Before integrating `dev/0.6.0`, assign its unfinished features a later release
number. Preserve the shipped schema v5 (billing addresses) and v6 (typed
billing-period expiry codes) artifacts. Renumber that branch's unreleased
schemas to v7 or later and provide an upgrade from v6; do not ship a different
schema under v5 or v6. If 0.5.x needs maintenance, branch from the last 0.5.x
revision.
Publishing from a maintenance branch requires an explicit update to the
workflow ref check and the `release` environment's allowed branches.

## Preflight

Update the workspace version, internal dependency requirements, `Cargo.lock`,
and `CHANGELOG.md`. Install the exact additional release tools when they are
not already available:

```console
rustup toolchain install 1.94.0 --profile minimal
cargo install sqlx-cli --version 0.9.0 --no-default-features --features postgres,rustls
cargo install cargo-audit --version 0.22.2 --locked
```

Then run:

```console
scripts/check-release.sh VERSION --allow-dirty
scripts/check-schema-immutability.sh
scripts/check-advisories.sh
scripts/check-public-api.sh
cargo +1.94.0 check --workspace --all-targets --locked
scripts/jig check contract
scripts/jig check fmt
scripts/jig check clippy
scripts/jig check test-locked
scripts/jig check sqlx
```

Replace `VERSION` with the exact stable semantic version recorded in the
workspace, such as `0.6.0`. The advisory check follows
[`security/dependency-advisories.md`](security/dependency-advisories.md).
Use SQLx CLI 0.9.0 for both metadata preparation and checks. To regenerate
metadata against the disposable PostgreSQL 18 schema-v6 database, run
`scripts/check-sqlx.sh --prepare` and review the `.sqlx` diff.

Commit the release preparation, push the release branch, wait for required CI
to pass, and rerun `scripts/check-release.sh VERSION` from the clean release
commit. Release the active stable series from `master`. Keep the separate
unfinished features on `dev/0.6.0` until their later release is ready.

## Trusted publishing

The preferred release path is the manual `Publish crates.io` GitHub Actions
workflow. It uses crates.io trusted publishing to obtain a short-lived token;
do not add a long-lived crates.io token to repository secrets.

One-time setup:

1. Create a GitHub environment named `release` and restrict it to the `master`
   deployment branch. Require a reviewer when the repository's GitHub plan
   supports deployment reviewers.
2. In the crates.io settings for each publishable crate, add the same GitHub
   trusted publisher:
   - repository owner: `bpcakes`
   - repository: `syrup-rail`
   - workflow: `release.yml`
   - environment: `release`
3. In GitHub Actions, run `Publish crates.io` from `master` and enter the version
   already recorded in the release commit.

The workflow validates metadata and package contents, runs the repository
gates, publishes in dependency order, waits for each package to become visible,
and finally creates and pushes the annotated `vVERSION` tag. A partially
completed workflow can be rerun: immutable crate versions already present on
crates.io are skipped, and the remaining packages continue in order.

## Local fallback

If trusted publishing is unavailable, authenticate Cargo through a configured
credential provider and run the preflight from the clean release branch. Publish each
crate in the order above with `cargo publish --locked -p CRATE`, checking its
package first with `--dry-run`. Push an annotated `vVERSION` tag only after all
four crate versions are visible on crates.io.
