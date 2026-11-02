#!/usr/bin/env bash
# Cuts a draft GitHub release of `zone` on a maintainer's machine; this repo has no CI. From a
# clean, pushed checkout it runs scripts/check.sh, builds every target (Linux, macOS, Windows),
# writes SHA256SUMS (signed when ZONE_SIGN_KEY is set, see scripts/sign.sh) and uploads them with
# install.sh to a draft release.
# A draft is not public and creates no tag; publishing it is a separate, approved step.
#
#   scripts/release.sh vX.Y.Z[-pre]
#   ZONE_RELEASE_APPROVED=<approval id> scripts/release.sh --publish vX.Y.Z[-pre]
#
# With ZONE_BUILD_LOCK set to a file, the gate and the builds hold flock on it, so they never
# overlap other gates on a shared machine. Needs gh, logged in with access to this repository.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

usage() {
  echo "usage: scripts/release.sh [--publish] vX.Y.Z[-pre]" >&2
  exit 2
}
fail() {
  echo "release: $*" >&2
  exit 1
}

publish=0
if [ "${1:-}" = --publish ]; then
  publish=1
  shift
fi
[ "$#" -eq 1 ] || usage
tag="$1"
[[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || usage

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
base="${tag#v}"
base="${base%%-*}"
[ "${base}" = "${version}" ] || fail "${tag} is not a tag for version ${version} in Cargo.toml"

if [ "${publish}" -eq 1 ]; then
  [ -n "${ZONE_RELEASE_APPROVED:-}" ] ||
    fail "publishing waits for a recorded approval; set ZONE_RELEASE_APPROVED=<approval id>"
  draft="$(gh release view "${tag}" --json isDraft --jq .isDraft 2>/dev/null)" ||
    fail "no release ${tag} to publish"
  [ "${draft}" = true ] || fail "${tag} is already published"
  gh release edit "${tag}" --draft=false
  echo "release: published ${tag} (approval ${ZONE_RELEASE_APPROVED})"
  exit 0
fi

[ -z "$(git status --porcelain)" ] || fail "the working tree is not clean"
if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  fail "tag ${tag} exists"
fi
rc=0
git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null || rc=$?
case "${rc}" in
  0) fail "tag ${tag} exists on origin" ;;
  2) ;;
  *) fail "cannot list origin's tags" ;;
esac
if gh release view "${tag}" >/dev/null 2>&1; then
  fail "a release ${tag} exists"
fi
head="$(git rev-parse HEAD)"
git fetch -q origin
[ -n "$(git branch -r --contains "${head}")" ] || fail "HEAD ${head} is not pushed to origin"

if [ -n "${ZONE_BUILD_LOCK:-}" ]; then
  exec 9>>"${ZONE_BUILD_LOCK}"
  echo "release: waiting for ${ZONE_BUILD_LOCK}" >&2
  flock 9
fi
scripts/check.sh
rm -rf dist
scripts/build.sh all
cp install.sh dist/
if [ -n "${ZONE_BUILD_LOCK:-}" ]; then
  flock -u 9
fi
scripts/sign.sh dist

sdk="$(dist/zone-"${version}"-x86_64-unknown-linux-musl --version)"
notes="${sdk}, built from ${head}. Check a download as the README's \"Verify a download\" says."
if [ -f dist/UNSIGNED ]; then
  notes+=" This release is UNSIGNED: no release key exists yet, so only SHA256SUMS can be checked."
fi
pre=()
if [[ "${tag}" == *-* ]]; then
  pre=(--prerelease)
fi
gh release create "${tag}" --draft "${pre[@]}" --target "${head}" --title "zone ${tag}" \
  --notes "${notes}" dist/*
echo "release: draft ${tag} at ${head} with $(find dist -maxdepth 1 -type f | wc -l) files"
