#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

mode="check"
if [[ "${1:-}" == "--prepare" ]]; then
  mode="prepare"
elif [[ -n "${1:-}" ]]; then
  echo "usage: $0 [--prepare]" >&2
  exit 2
fi

sqlx_version="$(cargo sqlx --version)"
if [[ "${sqlx_version##* }" != "0.9.0" ]]; then
  echo "SQLx CLI 0.9.0 is required; found: $sqlx_version" >&2
  echo "Install with: cargo install sqlx-cli --version 0.9.0 --no-default-features --features postgres,rustls" >&2
  exit 1
fi

cargo run \
  --locked \
  --manifest-path tools/sqlx-gate/Cargo.toml \
  -- \
  "--$mode"
