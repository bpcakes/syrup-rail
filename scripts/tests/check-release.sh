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
syrup-rail = { version = "0.3.0", path = "crates/syrup-rail" }
syrup-rail-nmi-client = { version = "0.3.0", path = "crates/syrup-rail-nmi-client" }
syrup-rail-postgres = { version = "0.3.0", path = "crates/syrup-rail-postgres" }
syrup-rail-nmi = { version = "0.3.0", path = "crates/syrup-rail-nmi" }
EOF
printf '## [0.3.0] - 2026-08-23\n' >"$fixture_root/CHANGELOG.md"

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
    printf 'LICENSE\nREADME.md\n'
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
    "$shell" "$subject" 0.3.0 "$@" >/dev/null

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

if [[ -x /bin/bash ]] && /bin/bash -c '[[ ${BASH_VERSINFO[0]} -eq 3 && ${BASH_VERSINFO[1]} -eq 2 ]]'; then
  run_case "bash-3.2-clean" /bin/bash false ""
  run_case "bash-3.2-dirty" /bin/bash true " --allow-dirty" --allow-dirty
elif [[ "$require_bash_32" == true ]]; then
  printf 'Bash 3.2 is required in this compatibility job.\n' >&2
  exit 1
else
  printf 'Skipping Bash 3.2 compatibility cases: /bin/bash is not Bash 3.2.\n'
fi

printf 'Release wrapper argument tests passed.\n'
