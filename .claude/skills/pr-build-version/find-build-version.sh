#!/usr/bin/env bash
# Find the "Build Version" annotation on the current PR's "CI" Actions run.
# Prints: "<version>\n<run_url>" on success. Polls up to 5 min for the annotation.
set -euo pipefail

ANNOTATION_TITLE="Build Version"
WORKFLOW="CI"
DEADLINE=$(( $(date +%s) + 300 ))   # 5 minutes
POLL_INTERVAL=10

repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)

# Resolve the PR for the current branch -> head commit SHA + PR url.
pr_json=$(gh pr view --json headRefOid,url 2>/dev/null || true)
sha=$(echo "$pr_json" | jq -r '.headRefOid // empty')
pr_url=$(echo "$pr_json" | jq -r '.url // empty')
if [ -z "${sha:-}" ]; then
  echo "error: no PR found for the current branch" >&2
  exit 2
fi

found_run_url=""

while :; do
  # Find the CI workflow run for this exact head commit.
  run_json=$(gh run list --repo "$repo" --workflow "$WORKFLOW" --commit "$sha" \
    --limit 1 --json databaseId,url 2>/dev/null || echo '[]')
  run_id=$(echo "$run_json" | jq -r '.[0].databaseId // empty')
  run_url=$(echo "$run_json" | jq -r '.[0].url // empty')

  if [ -n "$run_id" ]; then
    found_run_url="$run_url"
    # Job ids are check-run ids; scan each job's annotations for the title.
    job_ids=$(gh api --paginate "repos/$repo/actions/runs/$run_id/jobs" -q '.jobs[].id' 2>/dev/null || true)
    for jid in $job_ids; do
      version=$(gh api "repos/$repo/check-runs/$jid/annotations" \
        -q ".[] | select(.title == \"$ANNOTATION_TITLE\") | .message" 2>/dev/null | head -n1 || true)
      if [ -n "${version:-}" ]; then
        printf '%s\n%s\n%s\n' "$version" "$run_url" "$pr_url"
        exit 0
      fi
    done
  fi

  if [ "$(date +%s)" -ge "$DEADLINE" ]; then
    if [ -z "$found_run_url" ]; then
      echo "error: no CI run found for commit $sha within 5 min" >&2
      exit 3
    fi
    echo "error: timeout - '$ANNOTATION_TITLE' annotation not found within 5 min" >&2
    echo "run: $found_run_url" >&2
    exit 4
  fi

  sleep "$POLL_INTERVAL"
done
