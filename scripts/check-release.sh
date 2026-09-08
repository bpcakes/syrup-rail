#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/check-release.sh VERSION [--allow-dirty]
       scripts/check-release.sh --development [--allow-dirty]

Validate the workspace version, internal dependency requirements, changelog,
tag availability, lockfile, and package file sets for a Syrup Rail release.
Development mode checks the current workspace version and packages without
requiring a finalized release changelog or an unused release tag.
EOF
}

version="${1:-}"
allow_dirty=false
development=false

if [[ -z "$version" || "$version" == "-h" || "$version" == "--help" ]]; then
  usage
  [[ -n "$version" ]] && exit 0
  exit 2
fi

shift
if [[ "$version" == "--development" ]]; then
  development=true
fi
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

if [[ "$development" == false && ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
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

if [[ "$development" == true ]]; then
  version="$workspace_version"
  if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "Workspace version must be a stable semantic version." >&2
    exit 1
  fi
fi

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
  if ! grep -Fq "$crate = { version = \"=$version\"," Cargo.toml; then
    echo "Workspace dependency $crate is not pinned exactly to version $version." >&2
    exit 1
  fi

  manifest="crates/$crate/Cargo.toml"
  if ! grep -Fq 'version.workspace = true' "$manifest" ||
    ! grep -Fq 'license-file.workspace = true' "$manifest" ||
    ! grep -Fq 'readme = "README.md"' "$manifest" ||
    ! grep -Fq 'publish = true' "$manifest"; then
    echo "$manifest must inherit the workspace version and license file, declare its README, and remain publishable." >&2
    exit 1
  fi
done

unreleased_heading_count="$(grep -cFx '## [Unreleased]' CHANGELOG.md || true)"
if [[ "$unreleased_heading_count" -ne 1 ]]; then
  echo "CHANGELOG.md must contain exactly one canonical ## [Unreleased] heading." >&2
  exit 1
fi

if [[ "$development" == false ]]; then
  unreleased_body="$(awk '
  $0 == "## [Unreleased]" { in_unreleased = 1; next }
  in_unreleased && /^## \[/ { exit }
  in_unreleased && NF { print }
' CHANGELOG.md)"
  if [[ "$unreleased_body" != "_No unreleased changes._" ]]; then
    echo "CHANGELOG.md must have no unreleased changes before publishing v$version." >&2
    exit 1
  fi

  if ! grep -Fq "## [$version] -" CHANGELOG.md; then
    echo "CHANGELOG.md has no dated $version release heading." >&2
    exit 1
  fi

  if git rev-parse --quiet --verify "refs/tags/v$version" >/dev/null; then
    echo "Tag v$version already exists." >&2
    exit 1
  fi
fi

if [[ "$allow_dirty" == false ]] && [[ -n "$(git status --porcelain)" ]]; then
  echo "Release checks require a clean worktree; use --allow-dirty while preparing." >&2
  exit 1
fi

cargo metadata --locked --no-deps --format-version 1 >/dev/null

# Keep the empty clean-mode argument vector safe under `set -u` on Bash 3.2
# through 4.3.
set --
if [[ "$allow_dirty" == true ]]; then
  set -- --allow-dirty
fi

for crate in "${publishable_crates[@]}"; do
  package_files="$(cargo package --locked --list "$@" -p "$crate")"
  for required_file in LICENSE README.md; do
    if ! grep -Fqx "$required_file" <<<"$package_files"; then
      echo "$crate package does not contain $required_file." >&2
      exit 1
    fi
  done
done

if [[ "$development" == true ]]; then
  echo "Development metadata and package file sets are valid for v$version."
else
  echo "Release metadata and package file sets are ready for v$version."
fi
