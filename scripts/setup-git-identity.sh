#!/bin/sh
# Points this clone at the tracked hooks and commits as Black Path Labs. Repo-local config
# only; your global git config is not touched. Run it once per clone:
#
#   scripts/setup-git-identity.sh
set -eu
cd "$(dirname "$0")/.."

git config --local core.hooksPath .githooks
git config --local user.name "Black Path Labs"
git config --local user.email "admin@blackpathlabs.com"
chmod +x .githooks/* scripts/check-no-internals.sh

denylist="${ZONE_INTERNALS_DENYLIST:-${HOME}/.config/zone-client/internals-denylist}"
[ -r "${denylist}" ] ||
  echo "setup-git-identity: no denylist at ${denylist}; commits and pushes fail until it exists" >&2
echo "setup-git-identity: hooks .githooks, identity Black Path Labs <admin@blackpathlabs.com>"
