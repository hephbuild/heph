#!/usr/bin/env bash
# Watch the current PR's "CI" run for the `upload_artifacts` job (display name
# "Pre-release") and report its terminal outcome.
#
# On success, prints:   ok\n<run_url>\n<pr_url>
# On skip/failure, prints (to stderr) the reason and exits non-zero:
#   3 -> no CI run found for the head commit within the deadline
#   4 -> job was skipped
#   5 -> job failed / cancelled / timed_out (reason = failed step names)
#   6 -> deadline reached before the job completed
# Polls until the job reaches a terminal state (default cap 45 min).
set -euo pipefail

JOB_NAME="${JOB_NAME:-Pre-release}"   # display name of the `upload_artifacts` job
WORKFLOW="CI"
DEADLINE=$(( $(date +%s) + ${TIMEOUT_SECONDS:-2700} ))   # default 45 minutes
POLL_INTERVAL="${POLL_INTERVAL:-15}"

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
    # Fetch the target job for this run by its display name.
    job=$(gh api --paginate "repos/$repo/actions/runs/$run_id/jobs" \
      -q ".jobs[] | select(.name == \"$JOB_NAME\")" 2>/dev/null | jq -s '.[0] // empty')

    if [ -n "$job" ] && [ "$job" != "null" ]; then
      status=$(echo "$job" | jq -r '.status // empty')
      conclusion=$(echo "$job" | jq -r '.conclusion // empty')

      if [ "$status" = "completed" ]; then
        case "$conclusion" in
          success)
            printf 'ok\n%s\n%s\n' "$run_url" "$pr_url"
            exit 0
            ;;
          skipped)
            echo "error: '$JOB_NAME' (upload_artifacts) was skipped" >&2
            echo "reason: an upstream dependency did not succeed (the job runs only when the 'gen' codegen job succeeds)" >&2
            echo "run: $run_url" >&2
            exit 4
            ;;
          *)
            failed_steps=$(echo "$job" | jq -r \
              '[.steps[]? | select(.conclusion == "failure" or .conclusion == "cancelled" or .conclusion == "timed_out") | .name] | join(", ")')
            echo "error: '$JOB_NAME' (upload_artifacts) concluded '$conclusion'" >&2
            if [ -n "$failed_steps" ]; then
              echo "reason: failing step(s): $failed_steps" >&2
            fi
            echo "run: $run_url" >&2
            exit 5
            ;;
        esac
      fi
    fi
  fi

  if [ "$(date +%s)" -ge "$DEADLINE" ]; then
    if [ -z "$found_run_url" ]; then
      echo "error: no CI run found for commit $sha within the deadline" >&2
      exit 3
    fi
    echo "error: timeout - '$JOB_NAME' (upload_artifacts) did not complete within the deadline" >&2
    echo "run: $found_run_url" >&2
    exit 6
  fi

  sleep "$POLL_INTERVAL"
done
