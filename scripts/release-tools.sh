#!/usr/bin/env bash
# Fetches the pinned minisign that scripts/sign.sh and scripts/verify.sh use into a directory and
# checks the download against the SHA-256 recorded here. An already fetched file is re-checked,
# not re-downloaded. Prints the env line those scripts read.
#
#   scripts/release-tools.sh <dir>
set -euo pipefail

dir="${1:-}"
if [ -z "${dir}" ]; then
  echo "usage: scripts/release-tools.sh <dir>" >&2
  exit 2
fi
case "$(uname -sm)" in
  "Linux x86_64") arch=x86_64 ;;
  "Linux aarch64") arch=aarch64 ;;
  *)
    echo "release-tools: pins exist for Linux x86_64 and aarch64 hosts only" >&2
    exit 1
    ;;
esac

# The upstream release, which also verifies under its author's published minisign key
# RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3.
minisign_version=0.12
minisign_tar="minisign-${minisign_version}-linux.tar.gz"
minisign_url="https://github.com/jedisct1/minisign/releases/download/${minisign_version}/${minisign_tar}"
minisign_sha=9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73

mkdir -p "${dir}/dl"
dir="$(cd "${dir}" && pwd)"

if [ ! -f "${dir}/dl/${minisign_tar}" ]; then
  curl -fsSL -o "${dir}/dl/${minisign_tar}.part" "${minisign_url}"
  mv "${dir}/dl/${minisign_tar}.part" "${dir}/dl/${minisign_tar}"
fi
if ! echo "${minisign_sha}  ${dir}/dl/${minisign_tar}" | sha256sum -c --quiet; then
  echo "release-tools: ${minisign_tar} does not match its pinned SHA-256" >&2
  exit 1
fi

minisign_dir="${dir}/minisign-${minisign_version}"
bin="${minisign_dir}/minisign-linux/${arch}/minisign"
if [ ! -x "${bin}" ]; then
  mkdir -p "${minisign_dir}"
  # The archive carries macOS xattr headers that GNU tar warns about and skips.
  tar -C "${minisign_dir}" -xzf "${dir}/dl/${minisign_tar}" --warning=no-unknown-keyword
fi

echo "export ZONE_MINISIGN=\"${bin}\""
