#!/usr/bin/env bash
# Checks a dist directory that scripts/sign.sh wrote: SHA256SUMS.minisig must be a good minisign
# signature of SHA256SUMS under the release public key, every file must be listed in SHA256SUMS
# and every listed file must be present with its sum. An unsigned release fails unless
# --allow-unsigned is given; then only the sums are checked.
#
#   scripts/verify.sh [-P <public key>] [--allow-unsigned] <dist dir>
#
# The public key is -P, else ZONE_SIGN_PUBKEY. minisign is ZONE_MINISIGN or minisign on PATH;
# scripts/release-tools.sh fetches the pinned one.
set -euo pipefail

usage() {
  echo "usage: scripts/verify.sh [-P <public key>] [--allow-unsigned] <dist dir>" >&2
  exit 2
}

pubkey="${ZONE_SIGN_PUBKEY:-}"
allow_unsigned=0
dist=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -P)
      [ "$#" -ge 2 ] || usage
      pubkey="$2"
      shift 2
      ;;
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

if [ -f SHA256SUMS.minisig ]; then
  if [ -z "${pubkey}" ]; then
    echo "verify: no public key; pass -P or set ZONE_SIGN_PUBKEY" >&2
    exit 1
  fi
  minisign="${ZONE_MINISIGN:-minisign}"
  if ! "${minisign}" -V -q -P "${pubkey}" -m SHA256SUMS -x SHA256SUMS.minisig; then
    echo "verify: SHA256SUMS.minisig is not a good signature of SHA256SUMS" >&2
    exit 1
  fi
elif [ "${allow_unsigned}" -eq 1 ]; then
  echo "verify: ${dist} is UNSIGNED; checking sums only" >&2
else
  echo "verify: ${dist} has no SHA256SUMS.minisig; pass --allow-unsigned to check sums only" >&2
  exit 1
fi

listed="$(sed 's/^[0-9a-f]\{64\}  //' SHA256SUMS)"
while IFS= read -r -d '' f; do
  f="${f#./}"
  case "${f}" in
    SHA256SUMS | SHA256SUMS.minisig | UNSIGNED) continue ;;
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
