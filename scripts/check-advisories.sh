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
cargo audit "$@"
