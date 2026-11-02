#!/usr/bin/env bash
# Smoke-tests a published or draft release, not a local build: downloads the release's install.sh
# and runs it into an empty HOME (it checks SHA256SUMS, and the signature when there is one),
# then the installed `zone` joins a real zone with `join --kit` and `zone status` must report
# admitted. The zone's admitter must be running and hold a grant for agents.
#
#   ZONE_SMOKE_INVITE=bpzc1.… ZONE_SMOKE_KEY_FILE=kit.key scripts/smoke.sh vX.Y.Z[-pre]
#   ZONE_SMOKE_HOME=DIR scripts/smoke.sh --install-only vX.Y.Z[-pre]
#
# ZONE_SMOKE_KEY_FILE is the kit key (mode 0600). ZONE_SMOKE_TIMEOUT (default 300) bounds the
# wait for admission. ZONE_SMOKE_HOME replaces the throwaway HOME; it must be empty, and is kept.
# ZONE_GH=1 downloads with `gh release download`, which also reaches a private repository or a
# draft release with gh's login. Exit 0 and a last line `smoke: PASS …` only when admitted.
set -euo pipefail

repo="Black-Path-Labs/zone-client"

usage() {
  echo "usage: scripts/smoke.sh [--install-only] vX.Y.Z[-pre]" >&2
  exit 2
}
fail() {
  echo "smoke: $*" >&2
  exit 1
}

install_only=0
if [ "${1:-}" = --install-only ]; then
  install_only=1
  shift
fi
[ "$#" -eq 1 ] || usage
tag="$1"
[[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || usage
if [ "${install_only}" -eq 0 ]; then
  [ -n "${ZONE_SMOKE_INVITE:-}" ] || fail "set ZONE_SMOKE_INVITE to the zone code or invite"
  key="${ZONE_SMOKE_KEY_FILE:-}"
  [ -f "${key}" ] || fail "set ZONE_SMOKE_KEY_FILE to the kit key file (got '${key}')"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT INT TERM
if [ -n "${ZONE_SMOKE_HOME:-}" ]; then
  home="${ZONE_SMOKE_HOME}"
  mkdir -p "${home}"
  [ -z "$(ls -A "${home}")" ] || fail "ZONE_SMOKE_HOME ${home} is not empty"
else
  home="${tmp}/home"
  mkdir "${home}"
fi
# gh keeps its login under the real HOME; the install itself sees only the empty one.
gh_config="${GH_CONFIG_DIR:-${XDG_CONFIG_HOME:-${HOME}/.config}/gh}"

mkdir "${tmp}/dl"
if [ "${ZONE_GH:-}" = 1 ]; then
  GH_CONFIG_DIR="${gh_config}" gh release download "${tag}" -R "${repo}" -D "${tmp}/dl" \
    -p install.sh || fail "${tag} has no install.sh"
else
  curl -fsSL -o "${tmp}/dl/install.sh" \
    "https://github.com/${repo}/releases/download/${tag}/install.sh" ||
    fail "cannot download install.sh for ${tag}"
fi

env -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u ZONE_INSTALL_DIR HOME="${home}" \
  GH_CONFIG_DIR="${gh_config}" ZONE_VERSION="${tag}" sh "${tmp}/dl/install.sh" ||
  fail "install.sh failed"
bin="${home}/.local/bin/zone"
[ -x "${bin}" ] || fail "install.sh put no zone at ${bin}"
version="$("${bin}" --version)"
echo "smoke: installed ${bin} (${version})" >&2
[ "${install_only}" -eq 0 ] || exit 0

# The installed binary, its default state dir under the empty HOME, nothing from this checkout.
zone() {
  env -u XDG_DATA_HOME HOME="${home}" "${bin}" --key-file "${key}" "$@"
}
printf '%s\n' "${ZONE_SMOKE_INVITE}" | zone join --kit --timeout "${ZONE_SMOKE_TIMEOUT:-300}" - ||
  fail "zone join --kit exited $?"
status="$(zone status)" || fail "zone status failed"
echo "${status}"
grep -q '"state":"admitted"' <<<"${status}" || fail "not admitted: ${status}"
echo "smoke: PASS ${tag} (${version}) admitted"
