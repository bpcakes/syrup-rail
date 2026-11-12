#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
subject="$repo_root/scripts/check-advisories.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

# Bash 4.4 changed empty-array expansion under `set -u`. Keep a structural
# regression guard because CI's newer Bash cannot reproduce the 3.2 failure.
if grep -Eq '\$\{[[:alnum:]_]+\[@\]' "$subject"; then
  printf 'advisory wrapper must not expand arrays under set -u\n' >&2
  exit 1
fi
if [[ "$(grep -Fc 'cargo audit "$@"' "$subject")" -ne 1 ]]; then
  printf 'advisory wrapper must invoke cargo audit exactly once\n' >&2
  exit 1
fi

mkdir -p "$test_root/bin"

cat >"$test_root/bin/git" <<'EOF'
#!/usr/bin/env bash
if [[ "$*" != "rev-parse --show-toplevel" ]]; then
  printf 'unexpected git command: %s\n' "$*" >&2
  exit 1
fi
printf '%s\n' "$ADVISORY_TEST_REPO_ROOT"
EOF

cat >"$test_root/bin/cargo" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  tree)
    if [[ "$*" != "tree --locked --workspace --all-features --target all -e all --prefix none --format {p}" ]]; then
      printf 'unexpected cargo tree arguments: %s\n' "$*" >&2
      exit 1
    fi
    : >"$ADVISORY_TEST_TREE_CALLED"
    if [[ -n "${ADVISORY_TEST_TREE_OUTPUT:-}" ]]; then
      printf '%s\n' "$ADVISORY_TEST_TREE_OUTPUT"
    fi
    if [[ "${ADVISORY_TEST_TREE_EXIT:-0}" -ne 0 ]]; then
      printf 'fixture graph resolution failed\n' >&2
      exit "$ADVISORY_TEST_TREE_EXIT"
    fi
    ;;
  audit)
    shift
    printf '%s\n' "$*" >"$ADVISORY_TEST_AUDIT_ARGS"
    exit "${ADVISORY_TEST_AUDIT_EXIT:-0}"
    ;;
  *)
    printf 'unexpected cargo command: %s\n' "$*" >&2
    exit 1
    ;;
esac
EOF

chmod +x "$test_root/bin/git" "$test_root/bin/cargo"

prepare_case() {
  local name="$1"
  local rsa_locked="$2"
  local sqlite_locked="${3:-false}"
  local case_root="$test_root/$name"

  mkdir -p "$case_root"
  printf '[[package]]\nname = "other"\nversion = "1.0.0"\n' >"$case_root/Cargo.lock"
  if [[ "$rsa_locked" == true ]]; then
    printf '[[package]]\nname = "rsa"\nversion = "0.9.10"\n' >>"$case_root/Cargo.lock"
  fi
  if [[ "$sqlite_locked" == true ]]; then
    printf '[[package]]\nname = "libsqlite3-sys"\nversion = "0.30.1"\n' >>"$case_root/Cargo.lock"
  fi
  printf '%s\n' "$case_root"
}

run_success_case() {
  local name="$1"
  local rsa_locked="$2"
  local expected_args="$3"
  local expect_tree="$4"
  local sqlite_locked="${5:-false}"
  local case_root
  case_root="$(prepare_case "$name" "$rsa_locked" "$sqlite_locked")"
  local actual="$case_root/audit-args"
  local tree_called="$case_root/tree-called"

  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_TREE_OUTPUT="other v1.0.0
rsa-helper v1.0.0
libsqlite3-sys-helper v1.0.0" \
    ADVISORY_TEST_TREE_CALLED="$tree_called" \
    ADVISORY_TEST_AUDIT_ARGS="$actual" \
    PATH="$test_root/bin:$PATH" \
    "$subject"

  if [[ "$(cat "$actual")" != "$expected_args" ]]; then
    printf 'unexpected cargo audit arguments for %s\n' "$name" >&2
    printf 'expected: %s\n' "$expected_args" >&2
    printf 'actual:   %s\n' "$(cat "$actual")" >&2
    exit 1
  fi
  if [[ "$expect_tree" == true && ! -f "$tree_called" ]]; then
    printf 'cargo tree was not called for %s\n' "$name" >&2
    exit 1
  fi
  if [[ "$expect_tree" == false && -e "$tree_called" ]]; then
    printf 'cargo tree was unexpectedly called for %s\n' "$name" >&2
    exit 1
  fi
}

run_reachable_package_case() {
  local package="$1"
  local advisory="$2"
  local version="$3"
  local rsa_locked=false
  local sqlite_locked=false
  if [[ "$package" == rsa ]]; then rsa_locked=true; else sqlite_locked=true; fi
  local case_root
  case_root="$(prepare_case "reachable-$package-$version" "$rsa_locked" "$sqlite_locked")"
  local actual="$case_root/audit-args"
  local tree_called="$case_root/tree-called"
  local stderr="$case_root/stderr"

  set +e
  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_TREE_OUTPUT="other v1.0.0
$package v$version" \
    ADVISORY_TEST_TREE_CALLED="$tree_called" \
    ADVISORY_TEST_AUDIT_ARGS="$actual" \
    PATH="$test_root/bin:$PATH" \
    "$subject" 2>"$stderr"
  local status=$?
  set -e

  if [[ "$status" -eq 0 ]]; then
    printf 'reachable %s unexpectedly passed the advisory policy\n' "$package" >&2
    exit 1
  fi
  if [[ ! -f "$tree_called" ]]; then
    printf 'cargo tree was not called for reachable %s\n' "$package" >&2
    exit 1
  fi
  if [[ -e "$actual" ]]; then
    printf 'cargo audit was called after reachable %s was detected\n' "$package" >&2
    exit 1
  fi
  if ! grep -Fq "$advisory may not be ignored because a locked $package package is reachable" "$stderr"; then
    printf 'reachable %s failure omitted the policy diagnostic\n' "$package" >&2
    exit 1
  fi
}

run_audit_failure_case() {
  local sqlite_locked="${1:-false}"
  local case_root
  case_root="$(prepare_case "audit-failure-$sqlite_locked" false "$sqlite_locked")"
  local actual="$case_root/audit-args"
  local tree_called="$case_root/tree-called"

  set +e
  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_TREE_OUTPUT="" \
    ADVISORY_TEST_TREE_CALLED="$tree_called" \
    ADVISORY_TEST_AUDIT_ARGS="$actual" \
    ADVISORY_TEST_AUDIT_EXIT="42" \
    PATH="$test_root/bin:$PATH" \
    "$subject"
  local status=$?
  set -e

  if [[ "$status" -ne 42 ]]; then
    printf 'cargo audit failure exited with %s instead of 42\n' "$status" >&2
    exit 1
  fi
  local expected_args="--deny warnings"
  if [[ "$sqlite_locked" == true ]]; then
    expected_args="$expected_args --ignore RUSTSEC-2026-0459"
  fi
  if [[ "$(cat "$actual")" != "$expected_args" ]]; then
    printf 'cargo audit failure received unexpected arguments\n' >&2
    exit 1
  fi
  if [[ "$sqlite_locked" == false && -e "$tree_called" ]]; then
    printf 'cargo tree was unexpectedly called for audit failure\n' >&2
    exit 1
  fi
}

run_graph_failure_case() {
  local package="$1"
  local rsa_locked=false
  local sqlite_locked=false
  if [[ "$package" == rsa ]]; then rsa_locked=true; else sqlite_locked=true; fi
  local case_root
  case_root="$(prepare_case "graph-failure-$package" "$rsa_locked" "$sqlite_locked")"
  local status=0
  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_TREE_OUTPUT="other v1.0.0" \
    ADVISORY_TEST_TREE_EXIT=47 \
    ADVISORY_TEST_TREE_CALLED="$case_root/tree-called" \
    ADVISORY_TEST_AUDIT_ARGS="$case_root/audit-args" \
    PATH="$test_root/bin:$PATH" \
    "$subject" 2>"$case_root/stderr" || status=$?
  if [[ "$status" -ne 47 || ! -f "$case_root/tree-called" || -e "$case_root/audit-args" ]]; then
    printf 'graph resolution failure did not stop before audit for %s\n' "$package" >&2
    exit 1
  fi
  if ! grep -Fq 'fixture graph resolution failed' "$case_root/stderr"; then
    printf 'graph resolution stderr was lost for %s\n' "$package" >&2
    exit 1
  fi
}

run_success_case "rsa-absent" false "--deny warnings" false
run_success_case "rsa-unreachable" true "--deny warnings --ignore RUSTSEC-2023-0071" true
run_success_case "sqlite-unreachable" false "--deny warnings --ignore RUSTSEC-2026-0459" true true
run_success_case "both-unreachable" true "--deny warnings --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2026-0459" true true
run_reachable_package_case rsa RUSTSEC-2023-0071 0.9.10
run_reachable_package_case rsa RUSTSEC-2023-0071 0.9.11
run_reachable_package_case libsqlite3-sys RUSTSEC-2026-0459 0.30.1
# The exception is forbidden for any reachable version, including future or
# patched versions, rather than matching only the currently locked version.
run_reachable_package_case libsqlite3-sys RUSTSEC-2026-0459 0.39.0
run_reachable_package_case libsqlite3-sys RUSTSEC-2026-0459 99.0.0
run_graph_failure_case rsa
run_graph_failure_case libsqlite3-sys
run_audit_failure_case
run_audit_failure_case true

printf 'Dependency advisory wrapper tests passed.\n'
