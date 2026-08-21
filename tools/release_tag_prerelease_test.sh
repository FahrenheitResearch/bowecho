#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
classifier="$script_dir/release_tag_prerelease.sh"

expect_kind() {
  local tag="$1"
  local expected="$2"
  local actual
  actual="$(bash "$classifier" "$tag")"
  if [[ "$actual" != "$expected" ]]; then
    echo "expected $tag to classify as prerelease=$expected, got $actual" >&2
    exit 1
  fi
}

expect_kind v0.34.14-rc.1 true
expect_kind v1.2.3-alpha true
expect_kind v1.2.3-alpha.1+build.7 true
expect_kind v0.34.14 false
expect_kind v1.2.3+build.7 false

if bash "$classifier" 0.34.14-rc.1 >/dev/null 2>&1; then
  echo "expected a tag without the required v prefix to fail" >&2
  exit 1
fi

if bash "$classifier" v >/dev/null 2>&1; then
  echo "expected an empty v-prefixed version to fail" >&2
  exit 1
fi

echo "release tag prerelease classification tests passed"
