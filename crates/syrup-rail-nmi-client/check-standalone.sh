#!/usr/bin/env bash

set -euo pipefail

crate_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "$crate_root/../.." && pwd)"
standalone_copy="$(mktemp -d "${TMPDIR:-/tmp}/syrup-rail-nmi-client.XXXXXX")"
package_target="$standalone_copy/package-target"
standalone_cache="$workspace_root/.cache/syrup-rail-nmi-client-standalone"
read -r toolchain_checksum _ < <(rustc --version --verbose | cksum)
package_root="$standalone_cache/package/$toolchain_checksum"
test_target="$standalone_cache/target/$toolchain_checksum"

cleanup_standalone_copy() {
  rm -rf -- "$standalone_copy"
}
trap cleanup_standalone_copy EXIT

(
  cd "$workspace_root"
  # Package the workspace member with current stable Cargo. The unqualified
  # Cargo commands below intentionally honor RUSTUP_TOOLCHAIN (when set) so CI
  # can compile and test the extracted artifact at the crate's MSRV.
  CARGO_TARGET_DIR="$package_target" cargo +stable package \
    --allow-dirty \
    --locked \
    --no-verify \
    --package syrup-rail-nmi-client
)

package_archives=("$package_target"/package/syrup-rail-nmi-client-*.crate)
if [[ "${#package_archives[@]}" -ne 1 || ! -f "${package_archives[0]}" ]]; then
  echo "expected exactly one packaged syrup-rail-nmi-client archive" >&2
  exit 1
fi
package_archive="${package_archives[0]}"
# Keep the extracted package at one stable path. Cargo fingerprints path
# packages by location, so a fresh extraction directory on every run would
# accumulate unreachable copies of this crate's artifacts in the shared target.
rm -rf -- "$package_root"
mkdir -p "$package_root"
tar -xzf "$package_archive" -C "$package_root" --strip-components=1
# Make the extracted package its own workspace root even when TMPDIR happens
# to live below an unrelated Cargo workspace. Cargo may eventually preserve a
# package-owned workspace table, in which case the isolation fence is already
# present and must not be duplicated.
if ! grep -Eq '^[[:space:]]*\[workspace\][[:space:]]*$' "$package_root/Cargo.toml"; then
  printf '\n[workspace]\n' >>"$package_root/Cargo.toml"
fi

(
  # Run from the extracted package so workspace-local Cargo configuration
  # cannot leak into the standalone build through the caller's directory.
  cd "$package_root"
  # The stable package path makes this remove only the package's prior units;
  # compiler-keyed dependency artifacts remain available for the rebuild.
  CARGO_TARGET_DIR="$test_target" cargo clean --package syrup-rail-nmi-client --manifest-path Cargo.toml
  CARGO_TARGET_DIR="$test_target" cargo metadata --locked --format-version 1 --no-deps --manifest-path Cargo.toml >/dev/null
  CARGO_TARGET_DIR="$test_target" cargo test --locked --manifest-path Cargo.toml
)
