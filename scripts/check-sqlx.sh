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

cargo run \
  --locked \
  --example sqlx_prepare_check \
  --manifest-path crates/syrup-rail-postgres/Cargo.toml \
  --features schema-contract-test-support \
  -- \
  "--$mode"
