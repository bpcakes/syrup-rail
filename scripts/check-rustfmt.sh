#!/usr/bin/env bash

set -euo pipefail

if [[ ! -f Cargo.toml ]]; then
  printf '%s\n' 'No Cargo.toml found; skipping cargo fmt.'
  exit 0
fi

cargo fmt --all -- --check

# rustfmt does not traverse source fragments referenced through include!.
# Check tracked and new non-ignored sources, including include! fragments,
# without walking generated output or other ignored files.
git ls-files -z --cached --others --exclude-standard -- \
  'crates/**/*.rs' 'tools/**/*.rs' | while IFS= read -r -d '' source_file; do
  # Unstaged deletions remain in the index but are no longer source inputs.
  [[ -f "$source_file" ]] || continue
  rustfmt --check --edition 2024 "$source_file"
done
