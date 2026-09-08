#!/usr/bin/env bash
set -euo pipefail

# Workflows pass GitHub event fields through the environment, never shell code.
# A push can contain several commits; HEAD^ would miss all but the last one.
empty_tree="$(git hash-object -t tree /dev/null)"
case "${GITHUB_EVENT_NAME:-}" in
  push|pull_request|merge_group)
    base_ref="${CI_BASE_SHA:-}"
    if [[ ! "$base_ref" =~ ^[0-9a-f]{40}$ ]]; then
      echo "Missing or invalid base SHA for $GITHUB_EVENT_NAME." >&2
      exit 1
    fi
    if [[ "$base_ref" == 0000000000000000000000000000000000000000 ]]; then
      if [[ "$GITHUB_EVENT_NAME" != push ]]; then
        echo "Only a new-branch push can have an empty base SHA." >&2
        exit 1
      fi
      base_ref="$empty_tree"
    elif ! git cat-file -e "$base_ref^{commit}" 2>/dev/null; then
      # The prior tip of a force push may not be in the checkout's history.
      git fetch --no-tags origin "$base_ref" >&2
      git cat-file -e "$base_ref^{commit}"
    fi
    ;;
  workflow_dispatch)
    # Manual runs check the latest commit; initial commits check the whole tree.
    base_ref="$(git rev-parse --verify HEAD^ 2>/dev/null || printf '%s' "$empty_tree")"
    ;;
  *)
    echo "Unsupported CI event: ${GITHUB_EVENT_NAME:-<missing>}." >&2
    exit 1
    ;;
esac
printf '%s\n' "$base_ref"
