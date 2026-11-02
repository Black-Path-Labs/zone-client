#!/usr/bin/env bash
# Writes SHA256SUMS over every artifact in a dist directory and signs it with cosign and the
# release key into SHA256SUMS.cosign.bundle. The private key never leaves the signing machine:
# ZONE_COSIGN_KEY names its file, and cosign reads its passphrase from COSIGN_PASSWORD, or from
# the file ZONE_COSIGN_PASSWORD_FILE names. Nothing is sent to a transparency log. The new bundle
# is then verified against cosign.pub in this repository, so a key that is not the published one
# fails here.
#
#   scripts/sign.sh <dist dir>
#   ZONE_UNSIGNED=1 scripts/sign.sh <dist dir>   # sums only, and an UNSIGNED file
#   scripts/sign.sh --release vX.Y.Z[-pre]       # signs a draft release's sums in place
#
# --release downloads the draft's assets, checks them against its SHA256SUMS, signs, uploads
# SHA256SUMS.cosign.bundle and removes UNSIGNED. A published release is refused.
# cosign is ZONE_COSIGN or cosign on PATH; scripts/release-tools.sh fetches the pinned one.
set -euo pipefail

scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Tests point ZONE_COSIGN_PUB at their own key; a release is checked against the repo's.
pub="${ZONE_COSIGN_PUB:-${scripts}/../cosign.pub}"
cosign="${ZONE_COSIGN:-cosign}"

fail() {
  echo "sign: $*" >&2
  exit 1
}
usage() {
  echo "usage: scripts/sign.sh <dist dir> | --release vX.Y.Z[-pre]" >&2
  exit 2
}

# sign_sums <dist dir>: SHA256SUMS.cosign.bundle over <dist dir>/SHA256SUMS.
sign_sums() {
  local dist="$1"
  [ -n "${ZONE_COSIGN_KEY:-}" ] && [ -f "${ZONE_COSIGN_KEY}" ] ||
    fail "set ZONE_COSIGN_KEY to the release key file. Nothing was signed."
  if [ -z "${COSIGN_PASSWORD+set}" ]; then
    [ -n "${ZONE_COSIGN_PASSWORD_FILE:-}" ] && [ -f "${ZONE_COSIGN_PASSWORD_FILE}" ] ||
      fail "set COSIGN_PASSWORD or ZONE_COSIGN_PASSWORD_FILE for the release key. Nothing was signed."
    COSIGN_PASSWORD="$(cat "${ZONE_COSIGN_PASSWORD_FILE}")"
    export COSIGN_PASSWORD
  fi
  rm -f "${dist}/SHA256SUMS.cosign.bundle" "${dist}/UNSIGNED"
  "${cosign}" sign-blob --yes --key "${ZONE_COSIGN_KEY}" --use-signing-config=false \
    --tlog-upload=false --bundle "${dist}/SHA256SUMS.cosign.bundle" "${dist}/SHA256SUMS" \
    >/dev/null 2>&1 || fail "cosign could not sign with ZONE_COSIGN_KEY"
  "${cosign}" verify-blob --key "${pub}" --bundle "${dist}/SHA256SUMS.cosign.bundle" \
    --insecure-ignore-tlog=true "${dist}/SHA256SUMS" >/dev/null 2>&1 || {
    rm -f "${dist}/SHA256SUMS.cosign.bundle"
    fail "the new signature does not verify against cosign.pub; ZONE_COSIGN_KEY is not the release key"
  }
}

if [ "${1:-}" = --release ]; then
  [ "$#" -eq 2 ] || usage
  tag="$2"
  [[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || usage
  draft="$(gh release view "${tag}" --json isDraft --jq .isDraft)" || fail "no release ${tag}"
  [ "${draft}" = true ] || fail "${tag} is published; sign before publishing"
  work="$(mktemp -d)"
  trap 'rm -rf "${work}"' EXIT
  gh release download "${tag}" -D "${work}/dist"
  "${scripts}/verify.sh" --allow-unsigned "${work}/dist" >/dev/null
  sign_sums "${work}/dist"
  gh release upload "${tag}" --clobber "${work}/dist/SHA256SUMS.cosign.bundle"
  gh release delete-asset "${tag}" UNSIGNED --yes 2>/dev/null || true
  notes="$(gh release view "${tag}" --json body --jq .body)"
  gh release edit "${tag}" --notes "${notes%% This release is UNSIGNED*}"
  echo "sign: ${tag} SHA256SUMS signed with the release key" >&2
  exit 0
fi

dist="${1:-}"
if [ "$#" -ne 1 ] || [ ! -d "${dist}" ]; then
  usage
fi
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "${scripts}/../Cargo.toml" | head -1)"

artifacts=()
while IFS= read -r -d '' f; do
  case "${f#"${dist}"/}" in
    SHA256SUMS | SHA256SUMS.cosign.bundle | UNSIGNED) ;;
    *) artifacts+=("${f#"${dist}"/}") ;;
  esac
done < <(find "${dist}" -maxdepth 1 -type f -print0 | LC_ALL=C sort -z)
if [ "${#artifacts[@]}" -eq 0 ]; then
  fail "${dist} holds no artifacts"
fi

rm -f "${dist}/SHA256SUMS" "${dist}/SHA256SUMS.cosign.bundle" "${dist}/UNSIGNED"
(cd "${dist}" && sha256sum -- "${artifacts[@]}" >SHA256SUMS)

if [ "${ZONE_UNSIGNED:-}" = 1 ]; then
  echo "zone-client ${version}: SHA256SUMS is not signed yet; scripts/sign.sh --release signs it." \
    >"${dist}/UNSIGNED"
  echo "sign: ${dist}/SHA256SUMS over ${#artifacts[@]} artifacts, UNSIGNED (ZONE_UNSIGNED=1)" >&2
  exit 0
fi
sign_sums "${dist}"
echo "sign: ${dist}/SHA256SUMS over ${#artifacts[@]} artifacts, signed with the release key" >&2
