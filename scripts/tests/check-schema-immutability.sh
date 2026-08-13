#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
checker="$repo_root/scripts/check-schema-immutability.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

new_repo() {
  local name="$1"
  local case_root="$test_root/$name"
  git init --quiet "$case_root"
  git -C "$case_root" config user.name "Schema Policy Test"
  git -C "$case_root" config user.email "schema-policy@example.invalid"
  mkdir -p "$case_root/crates/syrup-rail-postgres/schema/v1"
  printf '%s\n' 'schema one' >"$case_root/crates/syrup-rail-postgres/schema/v1/install.sql"
  git -C "$case_root" add .
  git -C "$case_root" commit --quiet -m "Release schema v1"
  git -C "$case_root" tag v0.1.0
  printf '%s\n' "$case_root"
}

commit_all() {
  local case_root="$1"
  local message="$2"
  git -C "$case_root" add -A
  git -C "$case_root" commit --quiet -m "$message"
}

prerelease_root="$(new_repo prerelease)"
mkdir -p "$prerelease_root/crates/syrup-rail-postgres/schema/v2"
printf '%s\n' 'schema two draft' >"$prerelease_root/crates/syrup-rail-postgres/schema/v2/install.sql"
commit_all "$prerelease_root" "Add schema v2 draft"
printf '%s\n' 'schema two final' >"$prerelease_root/crates/syrup-rail-postgres/schema/v2/install.sql"
commit_all "$prerelease_root" "Finish schema v2"
(cd "$prerelease_root" && "$checker") >/dev/null

v1_mutation_root="$(new_repo v1-mutation)"
printf '%s\n' 'changed schema one' >"$v1_mutation_root/crates/syrup-rail-postgres/schema/v1/install.sql"
commit_all "$v1_mutation_root" "Mutate schema v1"
if output="$(cd "$v1_mutation_root" && "$checker" 2>&1)"; then
  echo "Schema policy accepted a mutation to released schema v1." >&2
  exit 1
fi
if [[ "$output" != *"schema/v1 differs from its first released snapshot at v0.1.0"* ]]; then
  echo "Schema-v1 mutation did not report its release boundary: $output" >&2
  exit 1
fi

untracked_root="$(new_repo untracked)"
printf '%s\n' 'late untracked file' >"$untracked_root/crates/syrup-rail-postgres/schema/v1/late.sql"
if output="$(cd "$untracked_root" && "$checker" 2>&1)"; then
  echo "Schema policy accepted an untracked addition to released schema v1." >&2
  exit 1
fi
if [[ "$output" != *$'??\tcrates/syrup-rail-postgres/schema/v1/late.sql'* ]]; then
  echo "Untracked schema-v1 addition was not identified: $output" >&2
  exit 1
fi

v2_mutation_root="$(new_repo v2-mutation)"
mkdir -p "$v2_mutation_root/crates/syrup-rail-postgres/schema/v2"
printf '%s\n' 'schema two' >"$v2_mutation_root/crates/syrup-rail-postgres/schema/v2/install.sql"
commit_all "$v2_mutation_root" "Release schema v2"
git -C "$v2_mutation_root" tag v0.2.0
printf '%s\n' 'late addition' >"$v2_mutation_root/crates/syrup-rail-postgres/schema/v2/late.sql"
commit_all "$v2_mutation_root" "Extend released schema v2"
if output="$(cd "$v2_mutation_root" && "$checker" 2>&1)"; then
  echo "Schema policy accepted an addition to released schema v2." >&2
  exit 1
fi
if [[ "$output" != *"schema/v2 differs from its first released snapshot at v0.2.0"* ]]; then
  echo "Schema-v2 addition did not report its release boundary: $output" >&2
  exit 1
fi

echo "Schema immutability policy tests passed."
