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
    if [[ "$*" != "tree --locked --workspace --all-features --target all -e all --prefix none --format {p}" &&
          "$*" != "tree --locked --workspace --exclude syrup-rail-sqlx-gate --all-features --target all -e normal,build --prefix none --format {p}" ]]; then
      printf 'unexpected cargo tree arguments: %s\n' "$*" >&2
      exit 1
    fi
    : >"$ADVISORY_TEST_TREE_CALLED"
    if [[ -n "${ADVISORY_TEST_TREE_OUTPUT:-}" ]]; then
      printf '%s\n' "$ADVISORY_TEST_TREE_OUTPUT"
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

cat >"$test_root/bin/date" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${ADVISORY_TEST_DATE:-2026-11-05}"
EOF

chmod +x "$test_root/bin/git" "$test_root/bin/cargo" "$test_root/bin/date"

prepare_case() {
  local name="$1"
  local rsa_locked="$2"
  local case_root="$test_root/$name"

  mkdir -p "$case_root"
  if [[ "$rsa_locked" == true ]]; then
    printf '[[package]]\nname = "rsa"\nversion = "0.9.10"\n' >"$case_root/Cargo.lock"
  else
    printf '[[package]]\nname = "other"\nversion = "1.0.0"\n' >"$case_root/Cargo.lock"
  fi
  printf '%s\n' "$case_root"
}

run_success_case() {
  local name="$1"
  local rsa_locked="$2"
  local expected_args="$3"
  local expect_tree="$4"
  local case_root
  case_root="$(prepare_case "$name" "$rsa_locked")"
  local actual="$case_root/audit-args"
  local tree_called="$case_root/tree-called"

  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_TREE_OUTPUT="" \
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

run_reachable_rsa_case() {
  local case_root
  case_root="$(prepare_case "reachable-rsa" true)"
  local actual="$case_root/audit-args"
  local tree_called="$case_root/tree-called"
  local stderr="$case_root/stderr"

  set +e
  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_TREE_OUTPUT="rsa v0.9.10" \
    ADVISORY_TEST_TREE_CALLED="$tree_called" \
    ADVISORY_TEST_AUDIT_ARGS="$actual" \
    PATH="$test_root/bin:$PATH" \
    "$subject" 2>"$stderr"
  local status=$?
  set -e

  if [[ "$status" -eq 0 ]]; then
    printf 'reachable rsa unexpectedly passed the advisory policy\n' >&2
    exit 1
  fi
  if [[ ! -f "$tree_called" ]]; then
    printf 'cargo tree was not called for reachable rsa\n' >&2
    exit 1
  fi
  if [[ -e "$actual" ]]; then
    printf 'cargo audit was called after reachable rsa was detected\n' >&2
    exit 1
  fi
  if ! grep -Fq 'RUSTSEC-2023-0071 may not be ignored because a locked rsa package is reachable' "$stderr"; then
    printf 'reachable rsa failure omitted the policy diagnostic\n' >&2
    exit 1
  fi
}

run_audit_failure_case() {
  local case_root
  case_root="$(prepare_case "audit-failure" false)"
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
  if [[ "$(cat "$actual")" != "--deny warnings" ]]; then
    printf 'cargo audit failure received unexpected arguments\n' >&2
    exit 1
  fi
  if [[ -e "$tree_called" ]]; then
    printf 'cargo tree was unexpectedly called for audit failure\n' >&2
    exit 1
  fi
}

run_success_case "rsa-absent" false "--deny warnings" false
run_success_case "rsa-unreachable" true "--deny warnings --ignore RUSTSEC-2023-0071" true
run_reachable_rsa_case
run_audit_failure_case

run_tar_case() {
  local name="$1" version="$2" today="$3" runtime_tree="$4" expected_status="$5"
  local case_root
  case_root="$(prepare_case "$name" false)"
  printf '[[package]]\nname = "astral-tokio-tar"\nversion = "%s"\n' "$version" >>"$case_root/Cargo.lock"
  set +e
  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_DATE="$today" \
    ADVISORY_TEST_TREE_OUTPUT="$runtime_tree" \
    ADVISORY_TEST_TREE_CALLED="$case_root/tree-called" \
    ADVISORY_TEST_AUDIT_ARGS="$case_root/audit-args" \
    ADVISORY_TEST_AUDIT_EXIT="${6:-0}" \
    PATH="$test_root/bin:$PATH" \
    "$subject" 2>"$case_root/stderr"
  local status=$?
  set -e
  if [[ "$status" -ne "$expected_status" ]]; then
    printf 'unexpected status %s for %s\n' "$status" "$name" >&2
    exit 1
  fi
  if [[ "$expected_status" == 1 ]]; then
    if [[ -e "$case_root/audit-args" ]]; then
      printf 'audit invoked after rejected exception for %s\n' "$name" >&2
      exit 1
    fi
  elif [[ "$(cat "$case_root/audit-args")" != "--deny warnings --ignore RUSTSEC-2026-0440" ]]; then
    printf 'unexpected exception arguments for %s\n' "$name" >&2
    exit 1
  fi
}

run_tar_case tar-tool-only 0.6.4 2026-12-04 "" 0
run_tar_case tar-runtime 0.6.4 2026-11-05 "astral-tokio-tar v0.6.4" 1
run_tar_case tar-expired 0.6.4 2026-12-05 "" 1
run_tar_case tar-version-changed 0.7.1 2026-11-05 "" 1
run_tar_case tar-other-advisory 0.6.4 2026-11-05 "" 42 42

run_sqlite_case() {
  local name="$1" version="$2" full_tree="$3" expected_status="$4" diagnostic="${5:-}"
  local case_root
  case_root="$(prepare_case "$name" false)"
  printf '[[package]]\nname = "libsqlite3-sys"\nversion = "%s"\n' "$version" >>"$case_root/Cargo.lock"
  set +e
  ADVISORY_TEST_REPO_ROOT="$case_root" \
    ADVISORY_TEST_TREE_OUTPUT="$full_tree" \
    ADVISORY_TEST_TREE_CALLED="$case_root/tree-called" \
    ADVISORY_TEST_AUDIT_ARGS="$case_root/audit-args" \
    PATH="$test_root/bin:$PATH" \
    "$subject" 2>"$case_root/stderr"
  local status=$?
  set -e
  if [[ "$status" -ne "$expected_status" ]]; then
    printf 'unexpected status %s for %s\n' "$status" "$name" >&2
    exit 1
  fi
  if [[ "$expected_status" == 1 ]]; then
    if [[ -e "$case_root/audit-args" ]]; then
      printf 'audit invoked after rejected exception for %s\n' "$name" >&2
      exit 1
    fi
    if ! grep -Fq "$diagnostic" "$case_root/stderr"; then
      printf 'rejected exception for %s omitted the policy diagnostic\n' "$name" >&2
      exit 1
    fi
  else
    if [[ ! -f "$case_root/tree-called" ]]; then
      printf 'cargo tree was not called for %s\n' "$name" >&2
      exit 1
    fi
    if [[ "$(cat "$case_root/audit-args")" != "--deny warnings --ignore RUSTSEC-2026-0459" ]]; then
      printf 'unexpected exception arguments for %s\n' "$name" >&2
      exit 1
    fi
  fi
}

run_sqlite_case sqlite-unreachable 0.30.1 "" 0
run_sqlite_case sqlite-reachable 0.30.1 "libsqlite3-sys v0.30.1" 1 \
  'RUSTSEC-2026-0459 may not be ignored because a locked libsqlite3-sys package is reachable'
run_sqlite_case sqlite-version-changed 0.31.0 "" 1 \
  'RUSTSEC-2026-0459 exception covers only an unreachable libsqlite3-sys 0.30.1.'

printf 'Dependency advisory wrapper tests passed.\n'
