#!/usr/bin/env bash
# Fetches the pinned cosign that scripts/sign.sh and scripts/verify.sh use into a directory and
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

# The upstream release's cosign_checksums.txt, which Sigstore signs with cosign itself.
cosign_version=3.1.4
case "$(uname -sm)" in
  "Linux x86_64")
    arch=amd64
    cosign_sha=424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa
    ;;
  "Linux aarch64")
    arch=arm64
    cosign_sha=ade83b7aac62e131cacfe96d958948b2150cc9efec46a570a2649b8e27dedad2
    ;;
  *)
    echo "release-tools: pins exist for Linux x86_64 and aarch64 hosts only" >&2
    exit 1
    ;;
esac
cosign_bin="cosign-linux-${arch}"
cosign_url="https://github.com/sigstore/cosign/releases/download/v${cosign_version}/${cosign_bin}"

mkdir -p "${dir}/cosign-${cosign_version}"
dir="$(cd "${dir}" && pwd)"
bin="${dir}/cosign-${cosign_version}/${cosign_bin}"

if [ ! -f "${bin}" ]; then
  curl -fsSL -o "${bin}.part" "${cosign_url}"
  mv "${bin}.part" "${bin}"
fi
if ! echo "${cosign_sha}  ${bin}" | sha256sum -c --quiet; then
  echo "release-tools: ${cosign_bin} does not match its pinned SHA-256" >&2
  exit 1
fi
chmod 0755 "${bin}"

echo "export ZONE_COSIGN=\"${bin}\""
