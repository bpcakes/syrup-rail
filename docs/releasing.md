# Releasing Syrup Rail

Syrup Rail releases all four publishable crates at the same version. Publish
them in dependency order:

1. `syrup-rail`
2. `syrup-rail-nmi-client`
3. `syrup-rail-postgres`
4. `syrup-rail-nmi`

The workspace's internal dependency requirements must exactly match the
release version. The four crates share payment-evidence semantics as well as
Rust APIs, so an apparently compatible patch-level mix can change conservative
diagnostic routing. In particular, the Postgres and NMI packages can use APIs
and policy added in the matching core release and must not claim compatibility
with an older or newer core package.

## Preflight

Update the workspace version, internal dependency requirements, `Cargo.lock`,
and `CHANGELOG.md`. Install the exact additional release tools when they are
not already available:

```console
rustup toolchain install 1.88.0 --profile minimal
cargo install cargo-audit --version 0.22.2 --locked
```

Then run:

```console
scripts/check-release.sh VERSION --allow-dirty
scripts/check-schema-immutability.sh
scripts/check-advisories.sh
scripts/check-public-api.sh
cargo +1.88.0 check --workspace --all-targets --locked
scripts/jig check contract
scripts/jig check fmt
scripts/jig check clippy
scripts/jig check test-locked
scripts/jig check sqlx
```

Replace `VERSION` with the exact stable semantic version recorded in the
workspace, such as `0.3.0`. The advisory check permits only the documented,
unreachable SQLx-MySQL advisory described in
[`security/dependency-advisories.md`](security/dependency-advisories.md), and
fails if that dependency becomes reachable from a workspace build.

Commit the release preparation, push `master`, wait for required CI to pass, and
rerun `scripts/check-release.sh VERSION` from the clean release commit.

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
credential provider and run the preflight from a clean `master`. Publish each
crate in the order above with `cargo publish --locked -p CRATE`, checking its
package first with `--dry-run`. Push an annotated `vVERSION` tag only after all
four crate versions are visible on crates.io.
