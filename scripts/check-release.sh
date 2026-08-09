#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/check-release.sh VERSION [--allow-dirty]

Validate the workspace version, internal dependency requirements, changelog,
tag availability, lockfile, and package file sets for a Syrup Rail release.
EOF
}

version="${1:-}"
allow_dirty=false

if [[ -z "$version" || "$version" == "-h" || "$version" == "--help" ]]; then
  usage
  [[ -n "$version" ]] && exit 0
  exit 2
fi

shift
while (($#)); do
  case "$1" in
    --allow-dirty)
      allow_dirty=true
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "VERSION must be a semantic version such as 0.1.1." >&2
  exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

workspace_version="$({
  awk '
    $0 == "[workspace.package]" { in_workspace_package = 1; next }
    in_workspace_package && /^\[/ { exit }
    in_workspace_package && /^version[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      print value
      exit
    }
  ' Cargo.toml
})"

if [[ "$workspace_version" != "$version" ]]; then
  echo "Workspace version is ${workspace_version:-<missing>}, expected $version." >&2
  exit 1
fi

publishable_crates=(
  syrup-rail
  syrup-rail-nmi-client
  syrup-rail-postgres
  syrup-rail-nmi
)

for crate in "${publishable_crates[@]}"; do
  if ! grep -Fq "$crate = { version = \"$version\"," Cargo.toml; then
    echo "Workspace dependency $crate does not require version $version." >&2
    exit 1
  fi

  manifest="crates/$crate/Cargo.toml"
  if ! grep -Fq 'version.workspace = true' "$manifest" ||
    ! grep -Fq 'publish = true' "$manifest"; then
    echo "$manifest must inherit the workspace version and remain publishable." >&2
    exit 1
  fi
done

if ! grep -Fq "## [$version] -" CHANGELOG.md; then
  echo "CHANGELOG.md has no dated $version release heading." >&2
  exit 1
fi

if git rev-parse --quiet --verify "refs/tags/v$version" >/dev/null; then
  echo "Tag v$version already exists." >&2
  exit 1
fi

if [[ "$allow_dirty" == false ]] && [[ -n "$(git status --porcelain)" ]]; then
  echo "Release checks require a clean worktree; use --allow-dirty while preparing." >&2
  exit 1
fi

cargo metadata --locked --no-deps --format-version 1 >/dev/null

package_dirty_args=()
if [[ "$allow_dirty" == true ]]; then
  package_dirty_args+=(--allow-dirty)
fi

for crate in "${publishable_crates[@]}"; do
  cargo package --locked --list "${package_dirty_args[@]}" -p "$crate" >/dev/null
done

echo "Release metadata and package file sets are ready for v$version."
