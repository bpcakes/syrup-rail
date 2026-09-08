#!/usr/bin/env bash
set -euo pipefail

subject="$(git rev-parse --show-toplevel)/scripts/ci-base-ref.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT
git init -q -b master "$test_root/repository"
cd "$test_root/repository"
git config user.name 'CI regression'
git config user.email 'ci@example.invalid'
printf 'initial\n' > initial.rs
git add .
git commit -qm initial
initial="$(git rev-parse HEAD)"
empty_tree="$(git hash-object -t tree /dev/null)"

check_base() {
  local event="$1" supplied="$2" expected="$3"
  local actual
  actual="$(GITHUB_EVENT_NAME="$event" CI_BASE_SHA="$supplied" bash "$subject")"
  if [[ "$actual" != "$expected" ]]; then
    echo "$event: expected $expected, got $actual" >&2
    exit 1
  fi
}

check_base workflow_dispatch '' "$empty_tree"
check_base push 0000000000000000000000000000000000000000 "$empty_tree"
printf 'first pushed change\n' > first.rs
git add .
git commit -qm first
first="$(git rev-parse HEAD)"
printf 'second pushed change\n' > second.rs
git add .
git commit -qm second
git update-ref refs/remotes/origin/master HEAD

# Reproduce a multi-commit master push with origin/master already at HEAD.
base="$(GITHUB_EVENT_NAME=push CI_BASE_SHA="$initial" bash "$subject")"
changed="$(git diff --name-only "$base" HEAD)"
if [[ "$changed" != $'first.rs\nsecond.rs' ]]; then
  echo "Push comparison missed changed files: $changed" >&2
  exit 1
fi
check_base pull_request "$initial" "$initial"
check_base merge_group "$first" "$first"
check_base workflow_dispatch '' "$first"

for event in push pull_request merge_group; do
  if GITHUB_EVENT_NAME="$event" CI_BASE_SHA='' bash "$subject" > /dev/null 2>&1; then
    echo "$event accepted missing event data" >&2
    exit 1
  fi
done
if GITHUB_EVENT_NAME=pull_request CI_BASE_SHA=0000000000000000000000000000000000000000 \
  bash "$subject" > /dev/null 2>&1; then
  echo 'Pull request accepted an empty base' >&2
  exit 1
fi

echo 'CI base-ref regressions passed.'
