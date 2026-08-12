#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

facades=(
  crates/syrup-rail/src/lib.rs
  crates/syrup-rail-postgres/src/lib.rs
  crates/syrup-rail-nmi/src/lib.rs
  crates/syrup-rail-nmi-client/src/lib.rs
)

find_public_reexport_globs() {
  perl -0777 -ne '
    while (/^[ \t]*pub[ \t]+use\b(.*?);/msg) {
      my $start = $-[0];
      my $body = $1;
      next unless $body =~ /\*/;
      my $prefix = substr($_, 0, $start);
      my $line = 1 + ($prefix =~ tr/\n//);
      print "$ARGV:$line: wildcard public re-export\n";
      $found = 1;
    }
    END { exit($found ? 0 : 1) }
  ' "$@"
}

require_readable_regular_files() {
  local path
  for path in "$@"; do
    if [[ ! -f "$path" || ! -r "$path" ]]; then
      echo "Required public facade is missing or unreadable: $path" >&2
      return 1
    fi
  done
}

run_policy_regressions() {
  local missing_path="$repo_root/.intentionally-missing-public-facade"
  local missing_message

  if ! printf 'pub use crate::{\n    Explicit,\n    *\n};\n' |
    find_public_reexport_globs - >/dev/null; then
    echo "Public API policy regression: multiline wildcard was not detected." >&2
    return 1
  fi
  if printf 'pub use crate::{Explicit, Other};\n' |
    find_public_reexport_globs - >/dev/null; then
    echo "Public API policy regression: explicit exports were classified as a wildcard." >&2
    return 1
  fi
  if missing_message="$(require_readable_regular_files "$missing_path" 2>&1)"; then
    echo "Public API policy regression: a missing facade was accepted." >&2
    return 1
  fi
  if [[ "$missing_message" != "Required public facade is missing or unreadable: $missing_path" ]]; then
    echo "Public API policy regression: missing-facade diagnostic changed." >&2
    return 1
  fi
}

run_policy_regressions
require_readable_regular_files "${facades[@]}"

if find_public_reexport_globs "${facades[@]}"; then
  echo "Public crate facades must enumerate exports explicitly." >&2
  exit 1
fi

RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo test --workspace --all-features --doc --locked
