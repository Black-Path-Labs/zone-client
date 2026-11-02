#!/usr/bin/env bash
# Checks a dist directory that scripts/sign.sh wrote: SHA256SUMS.cosign.bundle must be a cosign
# signature of SHA256SUMS by the release key (cosign.pub in this repository; no transparency log
# entry is expected), every file must be listed in SHA256SUMS and every listed file must be
# present with its sum. An unsigned release fails unless --allow-unsigned is given; then only the sums
# are checked.
#
#   scripts/verify.sh [--allow-unsigned] <dist dir>
#
# cosign is ZONE_COSIGN or cosign on PATH; scripts/release-tools.sh fetches the pinned one.
set -euo pipefail

usage() {
  echo "usage: scripts/verify.sh [--allow-unsigned] <dist dir>" >&2
  exit 2
}

# Tests point ZONE_COSIGN_PUB at their own key; a release is checked against the repo's.
pub="${ZONE_COSIGN_PUB:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/cosign.pub}"
allow_unsigned=0
dist=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --allow-unsigned)
      allow_unsigned=1
      shift
      ;;
    -*) usage ;;
    *)
      [ -z "${dist}" ] || usage
      dist="$1"
      shift
      ;;
  esac
done
if [ -z "${dist}" ] || [ ! -d "${dist}" ]; then
  usage
fi
cd "${dist}"

if [ ! -f SHA256SUMS ]; then
  echo "verify: ${dist} has no SHA256SUMS" >&2
  exit 1
fi

if [ -f SHA256SUMS.cosign.bundle ]; then
  cosign="${ZONE_COSIGN:-cosign}"
  if ! "${cosign}" verify-blob --key "${pub}" --bundle SHA256SUMS.cosign.bundle \
    --insecure-ignore-tlog=true SHA256SUMS >/dev/null 2>&1; then
    echo "verify: SHA256SUMS.cosign.bundle is not a good signature of SHA256SUMS by the release key" >&2
    exit 1
  fi
elif [ "${allow_unsigned}" -eq 1 ]; then
  echo "verify: ${dist} is UNSIGNED; checking sums only" >&2
else
  echo "verify: ${dist} has no SHA256SUMS.cosign.bundle; pass --allow-unsigned to check sums only" >&2
  exit 1
fi

listed="$(sed 's/^[0-9a-f]\{64\}  //' SHA256SUMS)"
while IFS= read -r -d '' f; do
  f="${f#./}"
  case "${f}" in
    SHA256SUMS | SHA256SUMS.cosign.bundle | UNSIGNED) continue ;;
  esac
  if ! grep -Fxq -- "${f}" <<<"${listed}"; then
    echo "verify: ${f} is not in SHA256SUMS" >&2
    exit 1
  fi
done < <(find . -maxdepth 1 -type f -print0)

if ! sha256sum -c --quiet --strict SHA256SUMS; then
  echo "verify: an artifact does not match SHA256SUMS" >&2
  exit 1
fi
echo "verify: ${dist} OK"
