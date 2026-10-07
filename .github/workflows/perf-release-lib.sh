#!/bin/bash
# Shared release-resolution helpers for the Perf workflow (perf.yml).
#
# heph's release tags are not commit SHAs — a tag is `git describe` plus the
# CI run_number that built the commit (see version.sh) — so "the release for
# commit X" always means finding X's master-push CI run first, re-deriving
# the tag from it, and checking that the tag was actually published. The
# baseline and the candidate both need exactly that, plus the same
# partial-download handling, hence one library rather than two copies that
# drift apart.
#
# Source it, don't execute it. Requires in the environment:
#   REPO            owner/name of the source repo (for run lookups)
#   GH_TOKEN        token for REPO's Actions API
#   ARTIFACTS_TOKEN token for the artifacts repo below
#
# Functions that cannot continue call `exit 1` after writing an actionable
# message to stderr, so a caller cannot accidentally proceed on a
# half-resolved release. Note that `exit` inside a command substitution ends
# only the substitution's subshell — call sites that capture output must
# still check the status (`out=$(resolve_spec …) || exit 1`).

ARTIFACTS_REPO="hephbuild/heph-artifacts-v1"
_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 0 = published, 1 = genuinely absent (404). Anything else — a bad or
# expired PAT, an outage — exits instead of returning 1: treating "cannot
# check" as "not there" turns a dead token into a confident diagnosis of the
# wrong system (a walk would report "no published release found" after
# examining nothing).
release_exists() {
  local err
  if err=$(GH_TOKEN="$ARTIFACTS_TOKEN" gh api "repos/$ARTIFACTS_REPO/releases/tags/$1" 2>&1 >/dev/null); then
    return 0
  fi
  if printf '%s' "$err" | grep -q "HTTP 404"; then
    return 1
  fi
  echo "artifacts-repo API error while checking release '$1': $err" >&2
  exit 1
}

# Echoes "run_number<TAB>status<TAB>conclusion" for the master-push CI run
# whose head commit is $1; fails if there is none.
#
# Push runs only: a PR run's tag derives from its ephemeral merge commit,
# not the head_sha the runs API exposes, so it cannot be re-derived here.
# The workflow-scoped endpoint avoids both a `.name=="CI"` string coupling
# and sharing its 50-result window with the other push-triggered workflows.
#
# The `select(. != null)` is load-bearing: with no matching run, `first` is
# null and `@tsv` would render it as the non-empty string "\t\t-", which
# reads as success and yields a run_number of "" three lines later.
#
# "The API call failed" and "there are no such runs" are separated for the
# same reason release_exists separates them: `gh api --jq` exits 0 on an
# empty result set, so a non-zero status is a real failure and must not be
# reported to the user as "this commit has no CI run".
run_for_sha() {
  local out
  out=$(gh api "repos/$REPO/actions/workflows/heph.yml/runs?head_sha=$1&event=push" \
    --jq '.workflow_runs | first | select(. != null) | [.run_number, .status, .conclusion // "-"] | @tsv') || {
    echo "failed to query CI runs for $1 — cannot resolve its release" >&2
    exit 1
  }
  [ -n "$out" ] || return 1
  printf '%s' "$out"
}

# The tag a given run_number ($1) publishing commit $2 produced.
version_for_run() {
  "$_LIB_DIR/version.sh" "$1" "$2"
}

# The tag commit $1's own master-push CI run published; fails if it has none.
version_for_sha() {
  local info
  info=$(run_for_sha "$1") || return 1
  version_for_run "${info%%$'\t'*}" "$1"
}

# Resolves $1 — a commit SHA or an exact release tag — to
# "version<TAB>sha" on stdout. $2 labels the role ("baseline"/"candidate")
# in the error messages. The sha is best-effort for the tag form: it is
# recovered from the tag's own `+g<hash>` suffix, which this checkout may
# not contain.
resolve_spec() {
  local spec="$1" label="$2" sha version info run status conclusion short

  if echo "$spec" | grep -qE '^[0-9a-fA-F]{7,40}$'; then
    sha=$(git rev-parse --verify --quiet "$spec^{commit}") || {
      echo "$label '$spec' looks like a commit SHA but is not a commit in this checkout — dispatch from a ref that contains it" >&2
      exit 1
    }
    # "No run" is a normal state rather than proof of the wrong branch: CI
    # runs once per push, on that push's head commit, so every other commit
    # in a batched push has no run and no release of its own.
    info=$(run_for_sha "$sha") || {
      echo "no master-push CI run found for $label commit $sha — it is either not on master, or landed in a batched push whose CI ran on a later commit; pick a commit that has a CI run, or pass an exact release tag" >&2
      exit 1
    }
    IFS=$'\t' read -r run status conclusion <<< "$info"
    # Guarded explicitly: `set -e` does not reach inside `$( )` (bash leaves
    # `inherit_errexit` off), so an unguarded assignment here would carry an
    # empty tag forward into the messages below.
    version=$(version_for_run "$run" "$sha") || {
      echo "could not derive the release tag for $label commit $sha from CI run #$run" >&2
      exit 1
    }
    if ! release_exists "$version"; then
      # Still running vs published-nothing are the two states worth telling
      # apart — one is "wait", the other is "this commit has no build".
      if [ "$status" != "completed" ]; then
        echo "CI run #$run for $label commit $sha is still $status — wait for its Pre-release job to publish $version, then re-run" >&2
      else
        echo "CI run #$run for $label commit $sha completed ($conclusion) but never published release $version" >&2
      fi
      exit 1
    fi
    printf '%s\t%s' "$version" "$sha"
    return 0
  fi

  if ! release_exists "$spec"; then
    # Anything that is not 7-40 hex reached here as a tag, so a 6-character
    # abbreviated SHA lands on this message; say what it would have taken to
    # be read as a commit instead.
    echo "requested $label release '$spec' not found in $ARTIFACTS_REPO (if you meant a commit, give at least 7 hex characters)" >&2
    exit 1
  fi
  short="${spec##*+g}"
  sha=""
  [ "$short" = "$spec" ] || sha=$(git rev-parse --verify --quiet "$short^{commit}" || true)
  printf '%s\t%s' "$spec" "$sha"
}

# Downloads the named assets of release $1 into directory $2, best-effort,
# and echoes one line per asset that did not land. Callers decide whether a
# miss degrades (baseline) or fails (candidate).
#
# The result is judged by filename because `gh release download` exits 0
# when only SOME of its patterns matched: without this, a partially
# published release reaches a bare `cp: cannot stat` two steps later
# instead of a message naming what is absent.
#
# `gh`'s own stdout is discarded so this function's return channel is
# structurally its own — callers read the missing list from stdout, and one
# future line of chatter from `gh` would otherwise flip every caller's
# `[ -z … ]` test. A non-zero `gh` status is reported (a transient 5xx is
# not the same fact as "the release does not have this asset") but does not
# itself decide the outcome; the filenames do.
fetch_assets() {
  local version="$1" dir="$2"
  shift 2
  local asset rc=0 patterns=() missing=()
  for asset in "$@"; do
    patterns+=(--pattern "$asset")
  done
  mkdir -p "$dir"
  GH_TOKEN="$ARTIFACTS_TOKEN" gh release download "$version" \
    --repo "$ARTIFACTS_REPO" --dir "$dir" "${patterns[@]}" >/dev/null || rc=$?
  for asset in "$@"; do
    [ -f "$dir/$asset" ] || missing+=("$asset")
  done
  if [ "$rc" -ne 0 ] && [ ${#missing[@]} -gt 0 ]; then
    echo "gh release download exited $rc for $version (see its output above)" >&2
  fi
  [ ${#missing[@]} -eq 0 ] || printf '%s\n' "${missing[@]}"
}
