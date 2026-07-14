#!/usr/bin/env bash

set -euo pipefail

# Standard BowEcho release executables are expected to remain comfortably
# below this ceiling. Large optional LUT/scattering resources belong in
# external data packs, not inside every platform binary.
readonly DEFAULT_MAX_BYTES=134217728 # 128 MiB

fail() {
  local message="$1"
  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    printf '::error title=Release executable size guard::%s\n' "$message"
  fi
  printf 'ERROR: %s\n' "$message" >&2
  exit 1
}

if (( $# < 1 || $# > 2 )); then
  fail "usage: $0 <raw-executable> [maximum-bytes]"
fi

binary_path="$1"
max_bytes="${2:-$DEFAULT_MAX_BYTES}"

case "$max_bytes" in
  '' | *[!0-9]*)
    fail "maximum-bytes must be a positive integer, got '$max_bytes'"
    ;;
esac
if (( max_bytes == 0 )); then
  fail "maximum-bytes must be greater than zero"
fi

if [[ ! -f "$binary_path" ]]; then
  fail "raw BowEcho shipping executable was not found at '$binary_path'"
fi

size_bytes="$(wc -c < "$binary_path")"
size_bytes="${size_bytes//[[:space:]]/}"
case "$size_bytes" in
  '' | *[!0-9]*)
    fail "could not determine the byte size of '$binary_path'"
    ;;
esac

if (( size_bytes > max_bytes )); then
  fail "raw BowEcho shipping executable '$binary_path' is $size_bytes bytes, above the $max_bytes-byte release cap (128 MiB by default). Do not embed large optional LUT/scattering bundles in standard binaries; publish them as optional external data packs loaded at runtime. See docs/SIGNING.md, 'Release executable size guard'."
fi

printf 'Release executable size OK: %s bytes <= %s bytes (%s)\n' \
  "$size_bytes" "$max_bytes" "$binary_path"
