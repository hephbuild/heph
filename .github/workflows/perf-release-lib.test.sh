#!/bin/bash
# Unit tests for perf-release-lib.sh — the release resolution the Perf
# workflow's baseline and candidate both go through.
#
# `gh` and `git` are stubbed on PATH, so this runs offline in about a second
# and can assert the cases that are awkward to provoke against the real API:
# an Actions API failure, an expired artifacts token, a partially published
# release. Those are exactly the ones whose handling is easy to regress,
# because the happy path keeps working when they break.
#
#   bash .github/workflows/perf-release-lib.test.sh
#
# Run under `set -u` deliberately: the library is sourced into steps that do
# not, but an unset-variable bug here should fail loudly rather than resolve
# to the empty string and pick the wrong release.

set -uo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STUB_DIR="$(mktemp -d)"
trap 'rm -rf "$STUB_DIR"' EXIT

PASS=0
FAIL=0

# The stubs read their scripted behaviour from these files, so each case
# rewrites them rather than redefining functions.
GH_RUNS_JSON="$STUB_DIR/runs.json"
GH_RUNS_RC="$STUB_DIR/runs.rc"
GH_RELEASE_ERR="$STUB_DIR/release.err"
GH_RELEASE_RC="$STUB_DIR/release.rc"
GH_DOWNLOAD_RC="$STUB_DIR/download.rc"
GH_DOWNLOAD_FILES="$STUB_DIR/download.files"

cat > "$STUB_DIR/gh" <<'STUB'
#!/bin/bash
# Stub: `gh api .../runs...`, `gh api .../releases/tags/...`, `gh release download`.
case "$1 ${2:-}" in
  "api "*"/runs"*)
    cat "$GH_RUNS_JSON"
    exit "$(cat "$GH_RUNS_RC")"
    ;;
  "release download"*)
    dir=""
    prev=""
    for a in "$@"; do
      [ "$prev" = "--dir" ] && dir="$a"
      prev="$a"
    done
    while read -r f; do
      [ -n "$f" ] || continue
      mkdir -p "$dir"
      echo stub > "$dir/$f"
    done < "$GH_DOWNLOAD_FILES"
    exit "$(cat "$GH_DOWNLOAD_RC")"
    ;;
  *)
    # `gh api repos/<artifacts>/releases/tags/<tag>`
    cat "$GH_RELEASE_ERR" >&2
    exit "$(cat "$GH_RELEASE_RC")"
    ;;
esac
STUB

cat > "$STUB_DIR/git" <<'STUB'
#!/bin/bash
# Stub: only `rev-parse --verify --quiet <spec>^{commit}` is used by the lib.
# Anything in KNOWN_COMMITS resolves to itself; everything else fails.
if [ "$1" = "rev-parse" ]; then
  spec="${*: -1}"
  spec="${spec%^\{commit\}}"
  for k in $KNOWN_COMMITS; do
    if [ "$k" = "$spec" ]; then echo "$k"; exit 0; fi
  done
  exit 1
fi
exit 0
STUB

cat > "$STUB_DIR/version.sh" <<'STUB'
#!/bin/bash
# Stub for the real version.sh: tag is derived from run number + sha.
[ "${VERSION_SH_RC:-0}" = "0" ] || exit "$VERSION_SH_RC"
echo "v1.2.3-build.9.$1+g${2:0:8}"
STUB

chmod +x "$STUB_DIR/gh" "$STUB_DIR/git" "$STUB_DIR/version.sh"
cp "$LIB_DIR/perf-release-lib.sh" "$STUB_DIR/perf-release-lib.sh"

export PATH="$STUB_DIR:$PATH"
export REPO="acme/heph"
export GH_TOKEN="t"
export ARTIFACTS_TOKEN="t"
export GH_RUNS_JSON GH_RUNS_RC GH_RELEASE_ERR GH_RELEASE_RC GH_DOWNLOAD_RC GH_DOWNLOAD_FILES
export KNOWN_COMMITS="abc1234def5678abc1234def5678abc1234def56"

# Defaults: one completed run, release published, download yields nothing.
reset_stubs() {
  echo "4242	completed	success" > "$GH_RUNS_JSON"
  echo 0 > "$GH_RUNS_RC"
  : > "$GH_RELEASE_ERR"
  echo 0 > "$GH_RELEASE_RC"
  echo 0 > "$GH_DOWNLOAD_RC"
  : > "$GH_DOWNLOAD_FILES"
  unset VERSION_SH_RC
}

# Runs `$1` (a snippet sourced after the library) in a subshell with `set -e`,
# capturing stdout+stderr and the exit status — the same shape a workflow step
# gives the library.
run_snippet() {
  (
    set -e
    # shellcheck disable=SC1090
    . "$STUB_DIR/perf-release-lib.sh"
    eval "$1"
  ) 2>&1
}

check() { # $1=name $2=expected-substring $3=actual $4=expected-rc $5=actual-rc
  if [[ "$3" == *"$2"* ]] && [ "$4" = "$5" ]; then
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
  else
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n       want rc=%s containing: %s\n       got  rc=%s: %s\n' \
      "$1" "$4" "$2" "$5" "$3"
  fi
}

# --- run_for_sha ------------------------------------------------------------

reset_stubs
echo '' > "$GH_RUNS_JSON"
out=$(run_snippet 'run_for_sha deadbeef && echo FOUND || echo NO-RUN'); rc=$?
check "run_for_sha: no runs returns 1 (not a false hit)" "NO-RUN" "$out" 0 $rc

# The regression this guards: `.workflow_runs | first` is null with no runs,
# and `@tsv` renders null as the non-empty string "\t\t-", which read as a
# successful lookup and produced `CI run #- ... is still  —`.
reset_stubs
echo '' > "$GH_RUNS_JSON"
out=$(run_snippet 'v=$(run_for_sha deadbeef) || v="(failed)"; echo "[$v]"'); rc=$?
check "run_for_sha: empty result is empty, not a tab-rendered null" "[(failed)]" "$out" 0 $rc

reset_stubs
echo 1 > "$GH_RUNS_RC"
echo 'gh: Server Error (HTTP 502)' > "$GH_RUNS_JSON"
out=$(run_snippet 'run_for_sha deadbeef; echo UNREACHABLE'); rc=$?
check "run_for_sha: API failure is not 'no such run'" "failed to query CI runs" "$out" 1 $rc

# --- release_exists ---------------------------------------------------------

reset_stubs
out=$(run_snippet 'release_exists v1 && echo YES'); rc=$?
check "release_exists: published" "YES" "$out" 0 $rc

reset_stubs
echo 1 > "$GH_RELEASE_RC"
echo 'gh: Not Found (HTTP 404)' > "$GH_RELEASE_ERR"
out=$(run_snippet 'release_exists v1 || echo ABSENT'); rc=$?
check "release_exists: 404 is absent" "ABSENT" "$out" 0 $rc

reset_stubs
echo 1 > "$GH_RELEASE_RC"
echo 'gh: Bad credentials (HTTP 401)' > "$GH_RELEASE_ERR"
out=$(run_snippet 'release_exists v1 || echo ABSENT; echo UNREACHABLE'); rc=$?
check "release_exists: 401 exits, never reads as absent" "artifacts-repo API error" "$out" 1 $rc

# --- resolve_spec -----------------------------------------------------------

reset_stubs
out=$(run_snippet 'resolve_spec abc1234def5678abc1234def5678abc1234def56 candidate'); rc=$?
check "resolve_spec: SHA resolves to its run's tag" \
  "v1.2.3-build.9.4242+gabc1234d	abc1234def5678abc1234def5678abc1234def56" "$out" 0 $rc

reset_stubs
out=$(run_snippet 'resolve_spec 0000000000000000000000000000000000000000 candidate'); rc=$?
check "resolve_spec: unknown SHA fails" "is not a commit in this checkout" "$out" 1 $rc

reset_stubs
echo '' > "$GH_RUNS_JSON"
out=$(run_snippet 'resolve_spec abc1234def5678abc1234def5678abc1234def56 baseline'); rc=$?
check "resolve_spec: SHA with no CI run explains batched pushes" \
  "landed in a batched push" "$out" 1 $rc

reset_stubs
echo "4242	in_progress	-" > "$GH_RUNS_JSON"
echo 1 > "$GH_RELEASE_RC"; echo 'gh: Not Found (HTTP 404)' > "$GH_RELEASE_ERR"
out=$(run_snippet 'resolve_spec abc1234def5678abc1234def5678abc1234def56 candidate'); rc=$?
check "resolve_spec: still-running run says wait" "is still in_progress" "$out" 1 $rc

reset_stubs
echo "4242	completed	failure" > "$GH_RUNS_JSON"
echo 1 > "$GH_RELEASE_RC"; echo 'gh: Not Found (HTTP 404)' > "$GH_RELEASE_ERR"
out=$(run_snippet 'resolve_spec abc1234def5678abc1234def5678abc1234def56 candidate'); rc=$?
check "resolve_spec: completed-but-unpublished is distinct from still-running" \
  "never published release" "$out" 1 $rc

reset_stubs
out=$(run_snippet 'resolve_spec v1.2.3-build.9.4242+gabc1234 baseline'); rc=$?
check "resolve_spec: tag form recovers the sha from +g" \
  "v1.2.3-build.9.4242+gabc1234	" "$out" 0 $rc

reset_stubs
echo 1 > "$GH_RELEASE_RC"; echo 'gh: Not Found (HTTP 404)' > "$GH_RELEASE_ERR"
out=$(run_snippet 'resolve_spec abc123 baseline'); rc=$?
check "resolve_spec: 6-hex is treated as a tag, and says so" \
  "at least 7 hex characters" "$out" 1 $rc

reset_stubs
export VERSION_SH_RC=3
out=$(run_snippet 'resolve_spec abc1234def5678abc1234def5678abc1234def56 candidate'); rc=$?
unset VERSION_SH_RC
check "resolve_spec: version.sh failure does not yield an empty tag" \
  "could not derive the release tag" "$out" 1 $rc

# --- fetch_assets -----------------------------------------------------------

reset_stubs
printf 'a\nb\n' > "$GH_DOWNLOAD_FILES"
out=$(run_snippet 'cd "$STUB_DIR"; m=$(fetch_assets v1 d-all a b); echo "[$m]"'); rc=$?
check "fetch_assets: all present reports nothing missing" "[]" "$out" 0 $rc

reset_stubs
printf 'a\n' > "$GH_DOWNLOAD_FILES"
out=$(run_snippet 'cd "$STUB_DIR"; m=$(fetch_assets v1 d-partial a b); echo "[$(tr "\n" " " <<< "$m")]"'); rc=$?
check "fetch_assets: partial names only what is absent" "[b ]" "$out" 0 $rc

reset_stubs
: > "$GH_DOWNLOAD_FILES"
out=$(run_snippet 'cd "$STUB_DIR"; m=$(fetch_assets v1 d-none a b); echo "[$(tr "\n" " " <<< "$m")]"'); rc=$?
check "fetch_assets: all absent names them all" "[a b ]" "$out" 0 $rc

# A download that fails outright must not be silently indistinguishable from
# a release that genuinely lacks the asset.
reset_stubs
: > "$GH_DOWNLOAD_FILES"
echo 1 > "$GH_DOWNLOAD_RC"
out=$(run_snippet 'cd "$STUB_DIR"; fetch_assets v1 d-err a >/dev/null'); rc=$?
check "fetch_assets: reports a non-zero gh status" "gh release download exited 1" "$out" 0 $rc

# The return channel must be the function's alone: chatter on gh's stdout
# would otherwise be read as a missing-asset name by every caller.
reset_stubs
printf 'a\n' > "$GH_DOWNLOAD_FILES"
cat > "$STUB_DIR/gh" <<'STUB'
#!/bin/bash
echo "gh: a new release of gh is available"   # stdout chatter
dir=""; prev=""
for a in "$@"; do [ "$prev" = "--dir" ] && dir="$a"; prev="$a"; done
mkdir -p "$dir"; echo stub > "$dir/a"
exit 0
STUB
chmod +x "$STUB_DIR/gh"
out=$(run_snippet 'cd "$STUB_DIR"; m=$(fetch_assets v1 d-chatter a); echo "[$m]"'); rc=$?
check "fetch_assets: gh stdout cannot pollute the missing list" "[]" "$out" 0 $rc

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
