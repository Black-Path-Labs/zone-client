#!/usr/bin/env bash
# Writes SHA256SUMS over every artifact in a dist directory and signs it with cosign keyless
# (Sigstore) into SHA256SUMS.sigstore.json. The signing identity is fixed: admin@blackpathlabs.com,
# signed in through Google. Its OIDC token comes from scripts/sigstore-token.sh (open a URL, sign
# in) unless ZONE_IDENTITY_TOKEN names a token file. The token's email is checked before cosign
# runs, so no other identity ever reaches Fulcio or the public Rekor log. The new bundle is then
# verified against that identity.
#
#   scripts/sign.sh <dist dir>
#   ZONE_UNSIGNED=1 scripts/sign.sh <dist dir>   # sums only, and an UNSIGNED file
#   scripts/sign.sh --release vX.Y.Z[-pre]       # signs a draft release's sums in place
#
# --release downloads the draft's assets, checks them against its SHA256SUMS, signs, uploads
# SHA256SUMS.sigstore.json and removes UNSIGNED. A published release is refused.
# cosign is ZONE_COSIGN or cosign on PATH; scripts/release-tools.sh fetches the pinned one.
set -euo pipefail

identity="admin@blackpathlabs.com"
issuer="https://accounts.google.com"
scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cosign="${ZONE_COSIGN:-cosign}"

fail() {
  echo "sign: $*" >&2
  exit 1
}
usage() {
  echo "usage: scripts/sign.sh <dist dir> | --release vX.Y.Z[-pre]" >&2
  exit 2
}

# The token's claims, from its unverified payload: Fulcio checks the signature itself.
claims() {
  local payload
  payload="$(cut -d. -f2 "$1" | tr '_-' '/+')"
  while [ $((${#payload} % 4)) -ne 0 ]; do payload="${payload}="; done
  openssl base64 -d -A <<<"${payload}"
}

# sign_sums <dist dir>: SHA256SUMS.sigstore.json over <dist dir>/SHA256SUMS.
sign_sums() {
  local dist="$1" tmp token email connector verified
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp}"' RETURN
  if [ -n "${ZONE_IDENTITY_TOKEN:-}" ]; then
    token="${ZONE_IDENTITY_TOKEN}"
  else
    token="${tmp}/token"
    "${scripts}/sigstore-token.sh" "${token}" >/dev/null
  fi
  email="$(claims "${token}" | jq -r '.email // empty')"
  connector="$(claims "${token}" | jq -r '.federated_claims.connector_id // "google"')"
  verified="$(claims "${token}" | jq -r '.email_verified // false')"
  if [ "${email}" != "${identity}" ] || [ "${verified}" != true ] || [ "${connector}" != google ]; then
    fail "the token is for '${email}' (${connector}); sign in with Google as ${identity}. Nothing was signed."
  fi
  rm -f "${dist}/SHA256SUMS.sigstore.json" "${dist}/UNSIGNED"
  "${cosign}" sign-blob --yes --identity-token "${token}" \
    --bundle "${dist}/SHA256SUMS.sigstore.json" "${dist}/SHA256SUMS" >/dev/null
  "${cosign}" verify-blob --bundle "${dist}/SHA256SUMS.sigstore.json" \
    --certificate-identity "${identity}" --certificate-oidc-issuer "${issuer}" \
    "${dist}/SHA256SUMS" >/dev/null 2>&1 ||
    fail "the new signature does not verify as ${identity} from ${issuer}"
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
  gh release upload "${tag}" --clobber "${work}/dist/SHA256SUMS.sigstore.json"
  gh release delete-asset "${tag}" UNSIGNED --yes 2>/dev/null || true
  notes="$(gh release view "${tag}" --json body --jq .body)"
  gh release edit "${tag}" --notes "${notes%% This release is UNSIGNED*}"
  echo "sign: ${tag} SHA256SUMS signed as ${identity}" >&2
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
    SHA256SUMS | SHA256SUMS.sigstore.json | UNSIGNED) ;;
    *) artifacts+=("${f#"${dist}"/}") ;;
  esac
done < <(find "${dist}" -maxdepth 1 -type f -print0 | LC_ALL=C sort -z)
if [ "${#artifacts[@]}" -eq 0 ]; then
  fail "${dist} holds no artifacts"
fi

rm -f "${dist}/SHA256SUMS" "${dist}/SHA256SUMS.sigstore.json" "${dist}/UNSIGNED"
(cd "${dist}" && sha256sum -- "${artifacts[@]}" >SHA256SUMS)

if [ "${ZONE_UNSIGNED:-}" = 1 ]; then
  echo "zone-client ${version}: SHA256SUMS is not signed yet; scripts/sign.sh --release signs it." \
    >"${dist}/UNSIGNED"
  echo "sign: ${dist}/SHA256SUMS over ${#artifacts[@]} artifacts, UNSIGNED (ZONE_UNSIGNED=1)" >&2
  exit 0
fi
sign_sums "${dist}"
echo "sign: ${dist}/SHA256SUMS over ${#artifacts[@]} artifacts, signed as ${identity}" >&2
