#!/usr/bin/env bash

set -euo pipefail

if [[ ! -f Cargo.toml ]]; then
  printf '%s\n' 'No Cargo.toml found; skipping cargo fmt.'
  exit 0
fi

cargo fmt --all -- --check

# rustfmt does not traverse source fragments referenced through include!.
# Check each tracked Rust source independently so future splits cannot escape
# the repository formatting gate.
while IFS= read -r -d '' source_file; do
  rustfmt --check --edition 2024 "$source_file"
done < <(find crates tools -type f -name '*.rs' -print0)
