#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
subject="$repo_root/scripts/check-release.sh"
test_root="$(mktemp -d)"
modern_bash="$(command -v bash)"
require_bash_32="${RELEASE_TEST_REQUIRE_BASH_32:-false}"
trap 'rm -rf "$test_root"' EXIT

fixture_root="$test_root/repository"
mkdir -p "$fixture_root/crates" "$test_root/bin"

cat >"$fixture_root/Cargo.toml" <<'EOF'
[workspace.package]
version = "0.3.0"

[workspace.dependencies]
syrup-rail = { version = "=0.3.0", path = "crates/syrup-rail" }
syrup-rail-nmi-client = { version = "=0.3.0", path = "crates/syrup-rail-nmi-client" }
syrup-rail-postgres = { version = "=0.3.0", path = "crates/syrup-rail-postgres" }
syrup-rail-nmi = { version = "=0.3.0", path = "crates/syrup-rail-nmi" }
EOF
cat >"$fixture_root/CHANGELOG.md" <<'EOF'
## [Unreleased]

_No unreleased changes._

## [0.3.0] - 2026-08-23
EOF

for crate in syrup-rail syrup-rail-nmi-client syrup-rail-postgres syrup-rail-nmi; do
  mkdir -p "$fixture_root/crates/$crate"
  cat >"$fixture_root/crates/$crate/Cargo.toml" <<'EOF'
[package]
version.workspace = true
license-file.workspace = true
readme = "README.md"
publish = true
EOF
done

cat >"$test_root/bin/git" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  "rev-parse --show-toplevel")
    printf '%s\n' "$RELEASE_TEST_REPO_ROOT"
    ;;
  "rev-parse --quiet --verify refs/tags/v0.3.0")
    if [[ "${RELEASE_TEST_TAG_EXISTS:-false}" == true ]]; then
      exit 0
    fi
    exit 1
    ;;
  "status --porcelain")
    if [[ "${RELEASE_TEST_DIRTY:-false}" == true ]]; then
      printf ' M CHANGELOG.md\n'
    fi
    ;;
  *)
    printf 'unexpected git command: %s\n' "$*" >&2
    exit 1
    ;;
esac
EOF

cat >"$test_root/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$RELEASE_TEST_CARGO_CALLS"
case "${1:-}" in
  metadata)
    if [[ "$*" != "metadata --locked --no-deps --format-version 1" ]]; then
      printf 'unexpected cargo metadata arguments: %s\n' "$*" >&2
      exit 1
    fi
    ;;
  package)
    if [[ "${RELEASE_TEST_MISSING_LICENSE:-false}" != true ]]; then
      printf 'LICENSE\n'
    fi
    printf 'README.md\n'
    ;;
  *)
    printf 'unexpected cargo command: %s\n' "$*" >&2
    exit 1
    ;;
esac
EOF

chmod +x "$test_root/bin/git" "$test_root/bin/cargo"

run_case() {
  local name="$1"
  local shell="$2"
  local dirty="$3"
  local expected_package_args="$4"
  shift 4
  local calls="$test_root/$name-cargo-calls"

  RELEASE_TEST_REPO_ROOT="$fixture_root" \
    RELEASE_TEST_DIRTY="$dirty" \
    RELEASE_TEST_CARGO_CALLS="$calls" \
    PATH="$test_root/bin:$PATH" \
    "$shell" "$subject" "${RELEASE_TEST_MODE:-0.3.0}" "$@" >/dev/null

  if [[ "$(sed -n '1p' "$calls")" != "metadata --locked --no-deps --format-version 1" ]]; then
    printf 'unexpected metadata invocation for %s\n' "$name" >&2
    exit 1
  fi
  if [[ "$(wc -l <"$calls" | tr -d ' ')" -ne 5 ]]; then
    printf 'release wrapper made an unexpected number of cargo calls for %s\n' "$name" >&2
    exit 1
  fi
  for crate in syrup-rail syrup-rail-nmi-client syrup-rail-postgres syrup-rail-nmi; do
    if ! grep -Fqx "package --locked --list${expected_package_args} -p $crate" "$calls"; then
      printf 'unexpected package invocation for %s in %s\n' "$crate" "$name" >&2
      exit 1
    fi
  done
}

run_case "modern-clean" "$modern_bash" false ""
run_case "modern-dirty" "$modern_bash" true " --allow-dirty" --allow-dirty
RELEASE_TEST_MODE=--development run_case "modern-development" "$modern_bash" false ""
RELEASE_TEST_MODE=--development run_case "modern-development-dirty" "$modern_bash" true " --allow-dirty" --allow-dirty

# A missing packaged license must fail in either mode, even when Cargo succeeds.
for mode in 0.3.0 --development; do
  if RELEASE_TEST_REPO_ROOT="$fixture_root" \
    RELEASE_TEST_CARGO_CALLS="$test_root/missing-license-calls" \
    RELEASE_TEST_MISSING_LICENSE=true \
    PATH="$test_root/bin:$PATH" \
    "$modern_bash" "$subject" "$mode" >"$test_root/missing-license-rejection" 2>&1; then
    printf 'checker accepted a package without LICENSE in mode %s\n' "$mode" >&2
    exit 1
  fi
  grep -Fqx 'syrup-rail package does not contain LICENSE.' "$test_root/missing-license-rejection"
done

# Reject each non-exact dependency before reaching Cargo, independently of the
# successful packaging cases above.
cp "$fixture_root/Cargo.toml" "$test_root/exact-Cargo.toml"
for crate in syrup-rail syrup-rail-nmi-client syrup-rail-postgres syrup-rail-nmi; do
  sed "/^$crate = /s/\"=0.3.0\"/\"0.3.0\"/" "$test_root/exact-Cargo.toml" >"$fixture_root/Cargo.toml"
  for mode in 0.3.0 --development; do
    calls="$test_root/$crate-$mode-invalid-cargo-calls"
    if RELEASE_TEST_REPO_ROOT="$fixture_root" \
      RELEASE_TEST_CARGO_CALLS="$calls" \
      PATH="$test_root/bin:$PATH" \
      "$modern_bash" "$subject" "$mode" >"$test_root/rejection" 2>&1; then
      printf 'checker accepted a non-exact dependency: %s in mode %s\n' "$crate" "$mode" >&2
      exit 1
    fi
    if ! grep -Fqx "Workspace dependency $crate is not pinned exactly to version 0.3.0." "$test_root/rejection" || [[ -e "$calls" ]]; then
      printf 'unexpected dependency rejection for %s in mode %s\n' "$crate" "$mode" >&2
      cat "$test_root/rejection" >&2
      exit 1
    fi
  done
done
cp "$test_root/exact-Cargo.toml" "$fixture_root/Cargo.toml"

cp "$fixture_root/CHANGELOG.md" "$test_root/release-CHANGELOG.md"
cat >"$fixture_root/CHANGELOG.md" <<'EOF'
## [Unreleased]

### Changed

- This change has not been assigned to the release.

## [0.3.0] - 2026-08-23
EOF
calls="$test_root/unreleased-invalid-cargo-calls"
if RELEASE_TEST_REPO_ROOT="$fixture_root" \
  RELEASE_TEST_CARGO_CALLS="$calls" \
  PATH="$test_root/bin:$PATH" \
  "$modern_bash" "$subject" 0.3.0 >"$test_root/unreleased-rejection" 2>&1; then
  printf 'release checker accepted unreleased changes\n' >&2
  exit 1
fi
if ! grep -Fqx "CHANGELOG.md must have no unreleased changes before publishing v0.3.0." "$test_root/unreleased-rejection" || [[ -e "$calls" ]]; then
  printf 'unexpected unreleased-change rejection\n' >&2
  cat "$test_root/unreleased-rejection" >&2
  exit 1
fi
cp "$test_root/release-CHANGELOG.md" "$fixture_root/CHANGELOG.md"

# Development checks permit unreleased work and do not need a dated release.
cat >"$fixture_root/CHANGELOG.md" <<'EOF'
## [Unreleased]

### Changed

- Work toward the next minor release.
EOF
RELEASE_TEST_MODE=--development run_case "unreleased-development" "$modern_bash" false ""
cp "$test_root/release-CHANGELOG.md" "$fixture_root/CHANGELOG.md"

cat >"$fixture_root/CHANGELOG.md" <<'EOF'
## [Upcoming]

_No unreleased changes._

## [0.3.0] - 2026-08-23
EOF
calls="$test_root/unreleased-heading-invalid-cargo-calls"
if RELEASE_TEST_REPO_ROOT="$fixture_root" \
  RELEASE_TEST_CARGO_CALLS="$calls" \
  PATH="$test_root/bin:$PATH" \
  "$modern_bash" "$subject" 0.3.0 >"$test_root/unreleased-heading-rejection" 2>&1; then
  printf 'release checker accepted a missing canonical Unreleased heading\n' >&2
  exit 1
fi
if ! grep -Fqx "CHANGELOG.md must contain exactly one canonical ## [Unreleased] heading." "$test_root/unreleased-heading-rejection" || [[ -e "$calls" ]]; then
  printf 'unexpected Unreleased-heading rejection\n' >&2
  cat "$test_root/unreleased-heading-rejection" >&2
  exit 1
fi
cp "$test_root/release-CHANGELOG.md" "$fixture_root/CHANGELOG.md"

RELEASE_TEST_TAG_EXISTS=true RELEASE_TEST_MODE=--development \
  run_case "tagged-development" "$modern_bash" false ""
if RELEASE_TEST_REPO_ROOT="$fixture_root" \
  RELEASE_TEST_TAG_EXISTS=true \
  PATH="$test_root/bin:$PATH" \
  "$modern_bash" "$subject" 0.3.0 >"$test_root/tag-rejection" 2>&1; then
  printf 'release checker accepted an existing release tag\n' >&2
  exit 1
fi
grep -Fqx 'Tag v0.3.0 already exists.' "$test_root/tag-rejection"

if [[ -x /bin/bash ]] && /bin/bash -c '[[ ${BASH_VERSINFO[0]} -eq 3 && ${BASH_VERSINFO[1]} -eq 2 ]]'; then
  run_case "bash-3.2-clean" /bin/bash false ""
  run_case "bash-3.2-dirty" /bin/bash true " --allow-dirty" --allow-dirty
  RELEASE_TEST_MODE=--development run_case "bash-3.2-development" /bin/bash false ""
elif [[ "$require_bash_32" == true ]]; then
  printf 'Bash 3.2 is required in this compatibility job.\n' >&2
  exit 1
else
  printf 'Skipping Bash 3.2 compatibility cases: /bin/bash is not Bash 3.2.\n'
fi

printf 'Release wrapper argument tests passed.\n'
