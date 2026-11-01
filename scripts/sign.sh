#!/usr/bin/env bash
# Writes SHA256SUMS over every artifact in a dist directory and signs it with minisign into
# SHA256SUMS.minisig. The secret key is the file ZONE_SIGN_KEY names, read only here; it must be
# a minisign key without a password. With no ZONE_SIGN_KEY the release is unsigned: SHA256SUMS is
# written, no signature, and an UNSIGNED file says so. No release key exists yet,
# so every release is unsigned until one does.
#
#   ZONE_SIGN_KEY=<secret key file> scripts/sign.sh <dist dir>
#   scripts/sign.sh <dist dir>   # unsigned
#
# minisign is ZONE_MINISIGN or minisign on PATH; scripts/release-tools.sh fetches the pinned one.
set -euo pipefail

dist="${1:-}"
if [ -z "${dist}" ] || [ ! -d "${dist}" ]; then
  echo "usage: scripts/sign.sh <dist dir>" >&2
  exit 2
fi
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$(dirname "${BASH_SOURCE[0]}")/../Cargo.toml" | head -1)"
cd "${dist}"

artifacts=()
while IFS= read -r -d '' f; do
  case "${f#./}" in
    SHA256SUMS | SHA256SUMS.minisig | UNSIGNED) ;;
    *) artifacts+=("${f#./}") ;;
  esac
done < <(find . -maxdepth 1 -type f -print0 | LC_ALL=C sort -z)
if [ "${#artifacts[@]}" -eq 0 ]; then
  echo "sign: ${dist} holds no artifacts" >&2
  exit 1
fi

rm -f SHA256SUMS SHA256SUMS.minisig UNSIGNED
sha256sum -- "${artifacts[@]}" >SHA256SUMS

if [ -z "${ZONE_SIGN_KEY:-}" ]; then
  echo "zone-client ${version}: no release key yet; SHA256SUMS is not signed." >UNSIGNED
  echo "sign: ${dist}/SHA256SUMS over ${#artifacts[@]} artifacts, UNSIGNED (no ZONE_SIGN_KEY)" >&2
  exit 0
fi
if [ ! -f "${ZONE_SIGN_KEY}" ]; then
  echo "sign: ZONE_SIGN_KEY names no file" >&2
  exit 1
fi
minisign="${ZONE_MINISIGN:-minisign}"
"${minisign}" -S -s "${ZONE_SIGN_KEY}" -m SHA256SUMS -x SHA256SUMS.minisig \
  -t "zone-client ${version} SHA256SUMS" </dev/null >/dev/null
echo "sign: ${dist}/SHA256SUMS over ${#artifacts[@]} artifacts, signed into SHA256SUMS.minisig" >&2
