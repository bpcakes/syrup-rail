#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
subject="$repo_root/scripts/check-rustfmt.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT
mkdir -p "$test_root/repository/crates/sample/target" "$test_root/bin"
cd "$test_root/repository"
git init -q
printf 'target/\n' > .gitignore
: > Cargo.toml
: > crates/sample/tracked.rs
: > 'crates/sample/new fragment.rs'
: > crates/sample/target/generated.rs
git add .gitignore Cargo.toml crates/sample/tracked.rs

cat > "$test_root/bin/cargo" <<'STUB'
#!/usr/bin/env bash
[[ "$*" == 'fmt --all -- --check' ]]
STUB
cat > "$test_root/bin/rustfmt" <<'STUB'
#!/usr/bin/env bash
[[ "$1 $2 $3" == '--check --edition 2024' ]] || exit 2
printf '%s\n' "$4" >> "$RUSTFMT_TEST_LOG"
[[ "$4" != "${RUSTFMT_TEST_REJECT:-}" ]]
STUB
chmod +x "$test_root/bin/cargo" "$test_root/bin/rustfmt"
export PATH="$test_root/bin:$PATH"
export RUSTFMT_TEST_LOG="$test_root/calls"
bash "$subject"
sort "$RUSTFMT_TEST_LOG" > "$test_root/actual"
printf '%s\n' 'crates/sample/new fragment.rs' 'crates/sample/tracked.rs' > "$test_root/expected"
diff -u "$test_root/expected" "$test_root/actual"
export RUSTFMT_TEST_REJECT='crates/sample/tracked.rs'
if bash "$subject"; then
  printf 'Formatting gate swallowed a per-file failure\n' >&2
  exit 1
fi
printf 'Rust formatting discovery tests passed.\n'
