#!/usr/bin/env bash
set -euo pipefail

if (($#)); then
  echo "Usage: scripts/check-schema-immutability.sh" >&2
  exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

schema_dir="crates/syrup-rail-postgres/schema"
if [[ ! -d "$schema_dir" ]]; then
  echo "Schema directory is missing: $schema_dir" >&2
  exit 1
fi

release_tags=()
while IFS= read -r tag; do
  if [[ "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] &&
    git merge-base --is-ancestor "$tag" HEAD; then
    release_tags+=("$tag")
  fi
done < <(git tag --list 'v*' --sort=version:refname)

declare -A shipped_at=()
for tag in "${release_tags[@]}"; do
  while IFS= read -r version_dir; do
    if [[ "$version_dir" =~ ^v[1-9][0-9]*$ ]] &&
      [[ -z "${shipped_at[$version_dir]+present}" ]]; then
      shipped_at["$version_dir"]="$tag"
    fi
  done < <(git ls-tree -d --name-only "$tag:$schema_dir" 2>/dev/null || true)
done

if ((${#shipped_at[@]} == 0)); then
  echo "No released schema artifact directories found; pre-release artifacts remain editable."
  exit 0
fi

mapfile -t shipped_dirs < <(printf '%s\n' "${!shipped_at[@]}" | sort -V)
violations=0
for version_dir in "${shipped_dirs[@]}"; do
  release_tag="${shipped_at[$version_dir]}"
  version_path="$schema_dir/$version_dir"
  mapfile -t untracked_files < <(
    git ls-files --others --exclude-standard -- "$version_path"
  )
  if ! git diff --quiet "$release_tag" -- "$version_path" ||
    ((${#untracked_files[@]})); then
    echo "$version_path differs from its first released snapshot at $release_tag:" >&2
    git diff --name-status --no-renames "$release_tag" -- "$version_path" >&2
    if ((${#untracked_files[@]})); then
      printf '??\t%s\n' "${untracked_files[@]}" >&2
    fi
    violations=$((violations + 1))
  fi
done

if ((violations)); then
  echo "Released schema artifact directories are immutable; add the next complete version and forward-only cutover instead." >&2
  exit 1
fi

echo "Released schema artifact directories match their first stable release tags."
