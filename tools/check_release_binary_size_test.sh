#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
guard="$script_dir/check_release_binary_size.sh"
tmp_dir="$(mktemp -d)"
payload="$tmp_dir/payload.bin"
output="$tmp_dir/output.txt"

cleanup() {
  rm -f -- "$payload" "$output"
  rmdir -- "$tmp_dir"
}
trap cleanup EXIT

printf '12345678' > "$payload"

# The cap is inclusive.
bash "$guard" "$payload" 8 > "$output"
grep -Fq '8 bytes <= 8 bytes' "$output"

# One byte over the cap must fail with the data-pack remediation.
if bash "$guard" "$payload" 7 > "$output" 2>&1; then
  echo 'expected an over-cap executable to fail' >&2
  exit 1
fi
grep -Fq 'optional external data packs' "$output"

# A missing shipping path must fail rather than silently skip the check.
if bash "$guard" "$tmp_dir/missing.bin" 8 > "$output" 2>&1; then
  echo 'expected a missing executable to fail' >&2
  exit 1
fi
grep -Fq 'was not found' "$output"

echo 'release executable size guard tests passed'
