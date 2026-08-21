#!/usr/bin/env bash

set -euo pipefail

if (( $# != 1 )); then
  echo "usage: $0 <v-prefixed-semver-tag>" >&2
  exit 2
fi

tag="$1"
if [[ "$tag" != v?* ]]; then
  echo "release tag must begin with 'v' and contain a version: '$tag'" >&2
  exit 2
fi

# The release workflow separately proves that this tag exactly matches Cargo's
# workspace version, so Cargo owns full SemVer validation. Here we only need to
# distinguish a prerelease suffix from build metadata: 1.2.3-rc.1+build.7 is a
# prerelease, while 1.2.3+build.7 is not.
version="${tag#v}"
without_build="${version%%+*}"
if [[ "$without_build" == *-* ]]; then
  printf 'true\n'
else
  printf 'false\n'
fi
