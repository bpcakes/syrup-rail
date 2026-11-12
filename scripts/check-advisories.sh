#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

reachable_package_lines() {
  local package_name="$1"
  awk -v package_name="$package_name" '$1 == package_name'
}

# See docs/security/dependency-advisories.md. Each exact advisory/package pair
# is conditional on every version being absent from the complete build graph.
# Build cargo-audit's arguments through the shell positional parameters so the
# empty-argument case remains safe under `set -u` on Bash 3.2 through 4.3.
set -- --deny warnings
graph_loaded=false
package_graph=""
for exception in 'rsa:RUSTSEC-2023-0071' 'libsqlite3-sys:RUSTSEC-2026-0459'; do
  package="${exception%%:*}"
  advisory="${exception#*:}"
  if ! grep -Fqx "name = \"$package\"" Cargo.lock; then
    continue
  fi
  if [[ "$graph_loaded" == false ]]; then
    # Keep graph resolution outside a filter: any Cargo failure stops the
    # wrapper before it can add an exception or invoke the audit.
    package_graph="$(cargo tree --locked --workspace --all-features --target all -e all \
      --prefix none --format '{p}')"
    graph_loaded=true
  fi
  reachable="$(printf '%s\n' "$package_graph" | reachable_package_lines "$package")"
  if [[ -n "$reachable" ]]; then
    echo "$advisory may not be ignored because a locked $package package is reachable:" >&2
    echo "$reachable" >&2
    exit 1
  fi
  set -- "$@" --ignore "$advisory"
done
cargo audit "$@"
