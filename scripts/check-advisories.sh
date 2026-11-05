#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

readonly unreachable_rsa_advisory="RUSTSEC-2023-0071"
readonly unreachable_rsa_package="rsa"

ignore_unreachable_rsa_advisory=false

reachable_package_lines() {
  local package_name="$1"
  awk -v package_name="$package_name" '$1 == package_name'
}

rsa_selector_probe_count="$(
  printf 'rsa v0.9.10\nother v1.0.0\nrsa v0.9.11\n' |
    reachable_package_lines "$unreachable_rsa_package" |
    awk 'END { print NR }'
)"
if [[ "$rsa_selector_probe_count" -ne 2 ]]; then
  echo "Dependency advisory policy regression: every rsa version must be selected." >&2
  exit 1
fi

if grep -Fqx 'name = "rsa"' Cargo.lock; then
  rsa_tree="$(
    cargo tree --locked --workspace --all-features --target all -e all \
      --prefix none --format '{p}' |
      reachable_package_lines "$unreachable_rsa_package"
  )"
  if [[ -n "$rsa_tree" ]]; then
    echo "$unreachable_rsa_advisory may not be ignored because a locked $unreachable_rsa_package package is reachable:" >&2
    echo "$rsa_tree" >&2
    exit 1
  fi
  ignore_unreachable_rsa_advisory=true
fi

# See docs/security/dependency-advisories.md. This exception is safe only while
# every locked rsa version is absent from the complete workspace build graph.
# Build cargo-audit's arguments through the shell positional parameters so the
# empty-argument case remains safe under `set -u` on Bash 3.2 through 4.3.
set -- --deny warnings
if [[ "$ignore_unreachable_rsa_advisory" == true ]]; then
  set -- "$@" --ignore "$unreachable_rsa_advisory"
fi

# Temporary test/tool-only maintenance exception; see the reachability and
# impact analysis in docs/security/dependency-advisories.md. Never exempt a
# runtime dependency, a changed version, or a different advisory for this crate.
if grep -Fqx 'name = "astral-tokio-tar"' Cargo.lock; then
  if [[ "$(date -u +%F)" > "2026-12-04" ]]; then
    echo "RUSTSEC-2026-0440 test/tool exception expired; replace or reassess the dependency." >&2
    exit 1
  fi
  tar_versions="$(awk '
    /^name = / { selected = ($0 == "name = \"astral-tokio-tar\"") }
    selected && /^version = / { print $3 }
  ' Cargo.lock)"
  if [[ "$tar_versions" != '"0.6.4"' ]]; then
    echo "RUSTSEC-2026-0440 exception covers only astral-tokio-tar 0.6.4." >&2
    exit 1
  fi
  runtime_tar="$(
    cargo tree --locked --workspace --exclude syrup-rail-sqlx-gate \
      --all-features --target all -e normal,build --prefix none --format '{p}' |
      reachable_package_lines astral-tokio-tar
  )"
  if [[ -n "$runtime_tar" ]]; then
    echo "RUSTSEC-2026-0440 may not be ignored for a runtime or build dependency." >&2
    exit 1
  fi
  set -- "$@" --ignore RUSTSEC-2026-0440
fi
cargo audit "$@"
