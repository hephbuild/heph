#!/usr/bin/env bash
# Publish the plugin manifests (`heph-<plugin>-plugin.json` and its `.sha256`
# sidecar) for every platform whose cdylib the pre-release holds right now.
#
# Every `build` leg runs this the moment its own files are uploaded, so a
# platform's plugins are installable as soon as that platform is built rather
# than when the slowest one is; `manifests` runs it once more after every build,
# as the backstop. Callers may overlap in any number and any order: the two
# linux legs typically finish within seconds of each other. A manifest's bytes,
# and so its `.sha256`, change as platforms land: a checksum pinned from a
# pre-release is final only once its body reads "complete".
#
# Why overlapping callers are safe. Each manifest is a pure function of the
# release's asset listing (`pluginmanifest -from-assets`: sorted, checksums
# taken from GitHub's own asset digests), and a caller exits 0 only when ONE
# listing shows the published manifest equal to the one built from that same
# listing. A caller that finds it stale uploads and lists again; it never
# uploads on its last attempt, so every exit follows a listing. Take the
# caller whose final listing is the latest of all:
#   - every leg uploaded its cdylibs before its own first listing, so that
#     latest listing holds every platform uploaded so far;
#   - every caller's writes precede its own final listing, hence precede this
#     one, so nothing overwrote what it saw afterwards.
# So the manifest left behind covers every platform any caller had uploaded.
#
# That rests on a listing showing every write that finished before it, which
# GitHub does not document. The cheap half of it is checked: $EXPECT_PLATFORMS
# names the platforms the caller knows are uploaded (a leg its own, `manifests`
# all of them), and a listing missing any of their cdylibs is treated as lagging
# and listed again, never written from. `manifests` therefore also fails rather
# than mark a release complete with a platform missing.
#
# Two callers clobbering the same asset at once is expected: `--clobber`
# deletes then uploads, so the loser's upload fails or its read finds the
# asset briefly missing. Either reads as "stale" and is retried.
#
# Exits 0 once current; non-zero after $PUBLISH_ATTEMPTS listings that never
# were.
set -euo pipefail

: "${VERSION:?VERSION must be set}"
: "${REPO:?REPO must be set}"
: "${EXPECT_PLATFORMS:?EXPECT_PLATFORMS must name the platforms already uploaded, as <os>_<arch>}"

attempts="${PUBLISH_ATTEMPTS:-20}"
backoff="${PUBLISH_BACKOFF_SECONDS:-3}"

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

(cd "$root/tools/pluginmanifest" && go build -o "$work/pluginmanifest" .)

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# `<digest>` of asset $1 in the listing: empty when listed without one, and
# the word `absent` when not listed at all.
published() {
  awk -F'\t' -v n="$1" '$1 == n { d = $2; found = 1 } END { print (found ? d : "absent") }' "$assets"
}

base="https://github.com/$REPO/releases/download/$VERSION"
assets="$work/assets.tsv"
release_id=

for attempt in $(seq 1 "$attempts"); do
  [ "$attempt" = 1 ] || sleep $((RANDOM % (backoff + 1)))

  if [ -z "$release_id" ] &&
    ! release_id=$(gh api "repos/$REPO/releases/tags/$VERSION" --jq .id); then
    release_id=
    echo "attempt $attempt: cannot look up the release; trying again" >&2
    continue
  fi

  # One page, never `--paginate`: pages taken while another job clobbers can
  # skip or repeat an asset. A run publishes about 33 assets; 100 or more fails
  # loudly rather than tearing. Only assets that finished uploading: one still
  # in flight has no digest, and its URL does not serve yet.
  if ! gh api "repos/$REPO/releases/$release_id/assets?per_page=100" \
    --jq '(if length >= 100 then "TOO-MANY-ASSETS" else empty end), (.[] | select(.state == "uploaded") | [.name, (.digest // "")] | @tsv)' \
    >"$assets"; then
    echo "attempt $attempt: cannot list the release's assets; trying again" >&2
    continue
  fi
  if grep -qx 'TOO-MANY-ASSETS' "$assets"; then
    echo "::error title=Plugin manifests::the release holds 100 or more assets; the listing would need pages" >&2
    exit 1
  fi

  # Every plugin with a cdylib on the release, by pattern: the legs' upload
  # step is the one list of plugins.
  plugins=$(awk -F'\t' '$1 ~ /^heph-[a-z0-9]+-plugin_[a-z0-9]+_[a-z0-9]+\.(so|dylib)$/ {
      n = $1; sub(/^heph-/, "", n); sub(/-plugin_.*/, "", n); print n }' "$assets" | sort -u)

  # A listing with no plugin at all is checked for `go`, so it reads as lagging
  # rather than vacuously complete.
  lagging=
  for platform in $EXPECT_PLATFORMS; do
    for p in ${plugins:-go}; do
      grep -qE "^heph-$p-plugin_${platform}\.(so|dylib)"$'\t' "$assets" ||
        lagging="$lagging heph-$p-plugin_$platform"
    done
  done
  if [ -n "$lagging" ]; then
    echo "attempt $attempt: the listing lacks${lagging}; listing again" >&2
    continue
  fi

  stale=()
  pending=
  built=1
  for p in $plugins; do
    out="$work/heph-$p-plugin.json"
    if ! "$work/pluginmanifest" -name "$p" -version "$VERSION" -prefix "heph-$p-plugin" \
      -from-assets "$assets" -url-base "$base" -out "$out" 2>"$work/err"; then
      echo "attempt $attempt: cannot build the $p manifest from this listing:" >&2
      cat "$work/err" >&2
      built=0
      break
    fi
    for f in "$out" "$out.sha256"; do
      have=$(published "${f##*/}")
      if [ -z "$have" ]; then
        pending="$pending ${f##*/}"
      elif [ "$have" != "sha256:$(sha256 "$f")" ]; then
        stale+=("$f")
      fi
    done
  done
  [ "$built" = 1 ] || continue
  if [ -n "$pending" ]; then
    echo "attempt $attempt:${pending} listed without a digest yet; listing again" >&2
    continue
  fi

  if [ "${#stale[@]}" -eq 0 ]; then
    echo "plugin manifests are current for: $(echo $plugins) on" \
      "$(awk -F'\t' '$1 ~ /^heph-[a-z0-9]+-plugin_/ { n = $1; sub(/^heph-[a-z0-9]+-plugin_/, "", n); sub(/\.[a-z]+$/, "", n); print n }' "$assets" | sort -u | paste -sd' ' -)"
    exit 0
  fi
  if [ "$attempt" = "$attempts" ]; then
    break
  fi
  echo "attempt $attempt: publishing ${stale[*]##*/}"
  # A failure here is most likely another job clobbering the same asset; the
  # next listing decides.
  gh release upload "$VERSION" -R "$REPO" --clobber "${stale[@]}" ||
    echo "attempt $attempt: upload failed; listing again" >&2
done

echo "::error title=Plugin manifests::still not current after $attempts attempts" >&2
exit 1
