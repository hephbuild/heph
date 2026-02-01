#!/bin/bash
set -e

# Optional first argument: build number
build_number="${1:-}"

# Get the most recent tag
latest_tag=$(git describe --tags --abbrev=0)

if [ -z "$latest_tag" ]; then
    echo "No tags found"
    exit 1
fi

# Get the full description of current commit relative to last tag
# --long ensures we always get the commit count and hash
# --always returns hash even if no tags exist
describe=$(git describe --tags --long --always)

# If we're exactly on a tag, just output the tag
if git describe --tags --exact-match 2>/dev/null >/dev/null; then
    echo "$latest_tag"
else
    # Parse the describe output
    # Format is like: 1.2.3-5-g36e65
    # We want to transform it to: 1.2.3-build.5+g36e65
    # If build_number is provided, append it: 1.2.3-build.5.123+g36e65

    # Extract components using sed
    version=$(echo "$describe" | sed -E 's/^(.*)-([0-9]+)-g([0-9a-f]+)$/\1/')
    commits=$(echo "$describe" | sed -E 's/^(.*)-([0-9]+)-g([0-9a-f]+)$/\2/')
    hash=$(echo "$describe" | sed -E 's/^(.*)-([0-9]+)-g([0-9a-f]+)$/\3/')

    if [ -n "$build_number" ]; then
        echo "$version-build.$commits.$build_number+g$hash"
    else
        echo "$version-build.$commits+g$hash"
    fi
fi