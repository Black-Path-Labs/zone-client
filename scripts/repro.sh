#!/usr/bin/env bash
# Builds one release target twice from nothing, in two fresh target dirs, and fails unless both
# binaries have the same SHA-256. Each build is cold, so this takes two full release builds.
#
#   scripts/repro.sh x86_64-unknown-linux-musl
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

target="${1:-}"
if [ -z "${target}" ]; then
  echo "usage: scripts/repro.sh <target>" >&2
  exit 2
fi

sums=()
for run in a b; do
  dir="target/repro-${run}"
  rm -rf "${dir}"
  CARGO_TARGET_DIR="${dir}" scripts/build.sh "${target}" >&2
  sums+=("$(sha256sum "${dir}/${target}/release/zone" | cut -d' ' -f1)")
done

if [ "${sums[0]}" != "${sums[1]}" ]; then
  echo "repro: ${target} is not reproducible: ${sums[0]} != ${sums[1]}" >&2
  exit 1
fi
echo "repro: ${target} ${sums[0]} from both builds"
