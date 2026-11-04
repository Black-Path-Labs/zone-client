#!/usr/bin/env bash
# Fails if this repo names anything internal: private hosts and IPs, server paths, internal
# service and product names, or AI-tool attribution in commit messages. It runs on a
# maintainer's machine from the git hooks; this repo has no GitHub Actions.
#
#   scripts/check-no-internals.sh [--staged] [--tree] [--history] [--commits=<revs>] [--releases]
#
# --staged          files staged for the next commit (pre-commit)
# --tree            every tracked file in the working tree (default)
# --history         every commit on every branch and tag: author, committer, message and diff
# --commits=<revs>  the same for the commits `git rev-list <revs>` lists (pre-push)
# --releases        the name and notes of every GitHub release, drafts included (needs gh)
#
# Generic patterns live here. The names of internal hosts, services and paths cannot, or this
# file would publish them, so they come from a private denylist outside the repo: one extended
# regex per line, '#' starts a comment. Its path is ZONE_INTERNALS_DENYLIST, default
# ~/.config/zone-client/internals-denylist. A missing denylist is an error, not a pass.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

denylist="${ZONE_INTERNALS_DENYLIST:-${HOME}/.config/zone-client/internals-denylist}"
if [ ! -r "${denylist}" ]; then
  echo "check-no-internals: no denylist at ${denylist}; ask a maintainer for it" >&2
  exit 2
fi

patterns="$(mktemp)"
trap 'rm -f "${patterns}"' EXIT
{
  # Private IPv4 ranges, cloud host names that embed an address, and absolute home paths.
  cat <<'EOF'
\b10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\b
\b192\.168\.[0-9]{1,3}\.[0-9]{1,3}\b
\b172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3}\b
\bip-[0-9]{1,3}-[0-9]{1,3}-[0-9]{1,3}(-[0-9]{1,3})?\b
\.(internal|lan|corp)\b
/home/[a-z][a-z0-9_-]*/
/Users/[A-Za-z][A-Za-z0-9_-]*/
EOF
  grep -vE '^[[:space:]]*(#|$)' "${denylist}"
} >"${patterns}"

ai_terms='claude|anthropic|cursor|copilot|chatgpt|gpt-[0-9]|openai|codex|gemini|grok|ai-generated|generated (with|by)|co-authored-by'
identity='Black Path Labs <admin@blackpathlabs.com>'
# Commits already on main whose author or committer is not ${identity}. Rewriting main to fix
# them would take a force-push, which main does not allow, so they stay and skip the identity
# check only; their messages and diffs are still checked. Full 40-character SHAs only.
#   82a849c: zone join long-poll (#4), landed by a GitHub rebase merge, which made the
#            maintainer's GitHub account its committer.
identity_allowlist=(
  82a849c8f78381ba2c4de3882dff6835c21fcbfb
)
found=0

report() {
  echo "check-no-internals: $1" >&2
  sed 's/^/  /' >&2
  found=1
}

scan_files() { # $1 = label; file names on stdin, NUL-separated
  local hits
  hits="$(xargs -0 -r grep -nHIE -f "${patterns}" -- 2>/dev/null || true)"
  [ -z "${hits}" ] || report "$1" <<<"${hits}"
}

scan_staged() {
  local hits
  hits="$(git grep --cached -nIE -f "${patterns}" || true)"
  [ -z "${hits}" ] || report "staged files name something internal" <<<"${hits}"
}

scan_history() { # $@ = rev-list arguments
  local c hits who
  [ "$#" -gt 0 ] || return 0
  for c in $(git rev-list "$@"); do
    who="$(git log -1 --format='%an <%ae>|%cn <%ce>' "${c}")"
    if [ "${who}" != "${identity}|${identity}" ] && [[ " ${identity_allowlist[*]} " != *" ${c} "* ]]; then
      report "commit ${c} is not by ${identity}" <<<"${who}"
    fi
    hits="$(git log -1 --format=%B "${c}" | grep -niE "${ai_terms}" || true)"
    [ -z "${hits}" ] || report "commit ${c} message names an AI tool" <<<"${hits}"
    hits="$(git log -1 --format=%B "${c}" | grep -nE -f "${patterns}" || true)"
    [ -z "${hits}" ] || report "commit ${c} message names something internal" <<<"${hits}"
    hits="$(git show --format= --no-color --unified=0 "${c}" | grep -E '^\+[^+]' |
      grep -nE -f "${patterns}" || true)"
    [ -z "${hits}" ] || report "commit ${c} adds lines that name something internal" <<<"${hits}"
  done
}

scan_tags() {
  local tag hits
  while read -r tag; do
    hits="$(git cat-file -p "${tag}" | grep -niE -e "${ai_terms}" -e '^tagger ' |
      grep -vF "tagger ${identity}" || true)"
    [ -z "${hits}" ] || report "tag ${tag} has another tagger or names an AI tool" <<<"${hits}"
  done < <(git for-each-ref --format='%(objecttype) %(refname)' refs/tags | awk '$1 == "tag" { print $2 }')
}

scan_releases() {
  local repo hits
  repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
  hits="$(gh api "repos/${repo}/releases" --paginate --jq '.[] | "\(.tag_name): \(.name)\n\(.body)"' |
    grep -nE -f "${patterns}" || true)"
  [ -z "${hits}" ] || report "release notes name something internal" <<<"${hits}"
  hits="$(gh api "repos/${repo}/releases" --paginate --jq '.[] | "\(.tag_name): \(.name)\n\(.body)"' |
    grep -niE "${ai_terms}" || true)"
  [ -z "${hits}" ] || report "release notes name an AI tool" <<<"${hits}"
}

[ "$#" -gt 0 ] || set -- --tree
for arg in "$@"; do
  case "${arg}" in
  --staged) scan_staged ;;
  --tree) scan_files "tracked files name something internal" < <(git ls-files -z) ;;
  --history)
    mapfile -t refs < <(git for-each-ref --format='%(refname)' refs/heads refs/remotes refs/tags)
    scan_history "${refs[@]}"
    scan_tags
    ;;
  --commits=*)
    read -ra revs <<<"${arg#--commits=}"
    scan_history "${revs[@]}"
    ;;
  --releases) scan_releases ;;
  *)
    echo "usage: scripts/check-no-internals.sh [--staged] [--tree] [--history] [--commits=<revs>] [--releases]" >&2
    exit 2
    ;;
  esac
done

if [ "${found}" -ne 0 ]; then
  echo "check-no-internals: FAILED; strip the lines above or move them back to the private repo" >&2
  exit 1
fi
echo "check-no-internals: clean ($*)"
