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

- `master` is the default branch and the active 0.5.x release line.
- `dev/0.6.0` contains unfinished 0.6.0 development.
- `main` and `integration/0.6.0` are historical references; target new work at
  one of the two active branches above.

Fix stable issues on `master` and carry applicable fixes forward into
`dev/0.6.0`. Both active branches run push CI. Publishing is restricted to
`master`; developing on the default branch does not publish a release.

When 0.6.0 is ready, integrate it into `master`. First preserve the shipped
0.5.x schema artifacts and provide forward upgrades for the 0.6.0 schema
changes. Both development lines currently have different unreleased v5
artifacts; whichever ships first establishes the immutable v5 schema.
Create `release/0.5` before the 0.6.0 integration if 0.5.x still needs support.
Publishing from a maintenance branch requires an explicit update to the
workflow ref check and the `release` environment's allowed branches.

## Preflight

Pull requests and pushes to `master` or `dev/0.6.0` run
`scripts/check-release.sh --development`. This checks the current workspace
version, exact internal dependencies, locked Cargo metadata, and each package's
Elastic-2.0 license metadata and packaged `LICENSE`, `NOTICE.md`, and README
without requiring the changelog to be finalized. For local work in progress,
add `--allow-dirty`.

The four crate directories carry regular copies of the workspace `LICENSE`
and `NOTICE.md` files. The release check requires each copy to match its
workspace original, so consumers receive the complete terms and notices on
every checkout platform. Keep these files in every distribution; third-party
material retains its own terms.

CI also runs `crates/syrup-rail-nmi-client/check-standalone.sh` with stable Rust
and Rust 1.88.0. It creates and extracts the client archive, then tests it outside
the workspace. The other crates depend on matching unpublished workspace
versions, so their registry-backed `cargo publish --dry-run` checks still run
in dependency order during publishing.

Update the workspace version, internal dependency requirements, `Cargo.lock`,
and `CHANGELOG.md`. Install the exact additional release tools when they are
not already available:

```console
rustup toolchain install 1.88.0 --profile minimal --component clippy
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
RUSTUP_TOOLCHAIN=1.88.0 scripts/jig check clippy
scripts/jig check test-locked
scripts/jig check sqlx
```

Replace `VERSION` with the exact stable semantic version recorded in the
workspace, such as `0.3.0`. The advisory check permits only the documented,
unreachable SQLx-MySQL advisory described in
[`security/dependency-advisories.md`](security/dependency-advisories.md), and
fails if that dependency becomes reachable from a workspace build.

The cognitive-complexity threshold is calibrated against Clippy 1.88.0, so the
release gate deliberately uses that exact evaluator. Newer Clippy versions may
change the heuristic independently of the code.

Commit the release preparation, push the release branch, wait for required CI
to pass, and rerun `scripts/check-release.sh VERSION` from the clean release
commit. Release the active stable series from `master`. Keep unfinished
0.6.0 features on `dev/0.6.0` until that version is ready for release.

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
