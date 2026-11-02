#!/usr/bin/env bash
# Signs and notarizes the macOS `zone` binaries on a Mac, then rewrites SHA256SUMS over the
# signed bytes (scripts/sign.sh). Each zone-*-apple-darwin binary is signed with the Developer ID
# Application identity, hardened runtime and a secure timestamp, checked with codesign --verify,
# zipped and submitted to Apple's notary service, which must answer Accepted. Apple's stapler
# cannot staple a bare Mach-O binary, so Gatekeeper fetches the ticket online on first run.
#
#   ZONE_MACOS_IDENTITY="Developer ID Application: <name> (<team id>)" \
#   ZONE_NOTARY_PROFILE=<notarytool keychain profile> scripts/macos-sign.sh <dist dir>
#   ... scripts/macos-sign.sh --release vX.Y.Z[-pre]
#
# --release downloads every asset of that draft release, signs and notarizes there, and uploads
# the macOS binaries and the new SHA256SUMS (and SHA256SUMS.minisig or UNSIGNED) in their place.
# A published release is refused. When the draft's sums were signed, ZONE_SIGN_KEY must be set so
# they stay signed. Make the profile once with `xcrun notarytool store-credentials`.
set -euo pipefail
scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  echo "usage: scripts/macos-sign.sh <dist dir> | --release vX.Y.Z[-pre]" >&2
  exit 2
}
fail() {
  echo "macos-sign: $*" >&2
  exit 1
}

tag=""
dist=""
case "${1:-}" in
  --release)
    [ "$#" -eq 2 ] || usage
    tag="$2"
    [[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || usage
    ;;
  "" | -*) usage ;;
  *)
    [ "$#" -eq 1 ] || usage
    dist="$1"
    [ -d "${dist}" ] || fail "${dist} is not a directory"
    ;;
esac

[ "$(uname -s)" = Darwin ] || fail "codesign and notarytool run only on macOS"
[ -n "${ZONE_MACOS_IDENTITY:-}" ] ||
  fail "set ZONE_MACOS_IDENTITY to the Developer ID Application identity"
[ -n "${ZONE_NOTARY_PROFILE:-}" ] ||
  fail "set ZONE_NOTARY_PROFILE to a notarytool keychain profile"
for tool in codesign xcrun ditto; do
  command -v "${tool}" >/dev/null || fail "${tool} is not on PATH; install the Xcode command line tools"
done

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

if [ -n "${tag}" ]; then
  command -v gh >/dev/null || fail "gh is not on PATH"
  draft="$(gh release view "${tag}" --json isDraft --jq .isDraft 2>/dev/null)" ||
    fail "no release ${tag}"
  [ "${draft}" = true ] || fail "${tag} is published; only a draft is re-signed"
  dist="${work}/dist"
  mkdir "${dist}"
  gh release download "${tag}" --dir "${dist}"
  if [ -f "${dist}/SHA256SUMS.minisig" ] && [ -z "${ZONE_SIGN_KEY:-}" ]; then
    fail "${tag} has signed sums; set ZONE_SIGN_KEY so the new SHA256SUMS is signed too"
  fi
fi

bins=()
while IFS= read -r -d '' f; do
  bins+=("${f}")
done < <(find "${dist}" -maxdepth 1 -type f -name 'zone-*-apple-darwin' -print0 | LC_ALL=C sort -z)
[ "${#bins[@]}" -gt 0 ] || fail "${dist} holds no zone-*-apple-darwin binary"

for bin in "${bins[@]}"; do
  name="$(basename "${bin}")"
  codesign --force --options runtime --timestamp --sign "${ZONE_MACOS_IDENTITY}" "${bin}"
  codesign --verify --strict --verbose=2 "${bin}"
  ditto -c -k --keepParent "${bin}" "${work}/${name}.zip"
  result="$(xcrun notarytool submit "${work}/${name}.zip" --keychain-profile "${ZONE_NOTARY_PROFILE}" \
    --wait --output-format json)"
  if ! grep -q '"status" *: *"Accepted"' <<<"${result}"; then
    echo "${result}" >&2
    fail "${name} was not accepted by the notary service; see: xcrun notarytool log <id>"
  fi
  echo "macos-sign: ${name} signed and notarized" >&2
done

"${scripts}/sign.sh" "${dist}"

if [ -n "${tag}" ]; then
  uploads=("${bins[@]}" "${dist}/SHA256SUMS")
  if [ -f "${dist}/SHA256SUMS.minisig" ]; then
    uploads+=("${dist}/SHA256SUMS.minisig")
    gh release delete-asset "${tag}" UNSIGNED --yes 2>/dev/null || true
  else
    uploads+=("${dist}/UNSIGNED")
  fi
  gh release upload "${tag}" --clobber "${uploads[@]}"
  echo "macos-sign: ${tag} carries ${#bins[@]} notarized macOS binaries and new sums" >&2
fi
