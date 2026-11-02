#!/bin/sh
# Installs the `zone` client: downloads the release binary for this OS and CPU with its
# SHA256SUMS, checks the signature of SHA256SUMS against the release key (with cosign, or with
# openssl when cosign is not installed) and the binary's sum, and puts it in ~/.local/bin/zone.
# It never uses sudo.
#
#   curl -fsSL https://github.com/Black-Path-Labs/zone-client/releases/latest/download/install.sh | sh
#   curl -fsSL .../install.sh | ZONE_VERSION=v0.1.0 sh      # a pinned release
#
# ZONE_INSTALL_DIR replaces ~/.local/bin. ZONE_GH=1 downloads with `gh release download`, which
# also reaches a private repository or a draft release with gh's login. An unsigned release is
# refused unless ZONE_ALLOW_UNSIGNED=1. ZONE_COSIGN_PUB replaces the release key (tests).
set -eu

repo="Black-Path-Labs/zone-client"
# The release key: cosign.pub in the repository. Releases are signed with cosign and no
# transparency log entry.
release_key="-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE9ZhqkzNFhmlDIQ/ge7Z3WmXb0zxF
U/JP3rZ6s5hwWi9O6OVRS2GytBUEvoHmrA+wdaPSRnxWUkrD3ULPayjFlw==
-----END PUBLIC KEY-----"

fail() {
  echo "zone install: $*" >&2
  exit 1
}

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) fail "no release for $(uname -s) yet; build from source: https://github.com/${repo}" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "no release for $(uname -m) yet; build from source: https://github.com/${repo}" ;;
esac
target="${arch}-${os}"

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "needs sha256sum or shasum"
fi

version="${ZONE_VERSION:-}"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT INT TERM

fetch() {
  if [ -n "${version}" ]; then
    base="https://github.com/${repo}/releases/download/${version}"
  else
    base="https://github.com/${repo}/releases/latest/download"
  fi
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "${tmp}/$1" "${base}/$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "${tmp}/$1" "${base}/$1"
  else
    fail "needs curl or wget"
  fi
}

if [ "${ZONE_GH:-}" = 1 ]; then
  gh release download ${version:+"${version}"} -R "${repo}" -D "${tmp}" \
    -p SHA256SUMS -p 'SHA256SUMS.cosign.bundle' -p "zone-*-${target}" ||
    fail "gh release download failed"
else
  fetch SHA256SUMS || fail "cannot download SHA256SUMS"
  fetch SHA256SUMS.cosign.bundle 2>/dev/null || rm -f "${tmp}/SHA256SUMS.cosign.bundle"
fi

name="$(sed -n "s/^[0-9a-f]\{64\}  \(zone-[^ ]*-${target}\)\$/\1/p" "${tmp}/SHA256SUMS" | head -1)"
[ -n "${name}" ] || fail "the release has no zone binary for ${target}"
want="$(sed -n "s/^\([0-9a-f]\{64\}\)  ${name}\$/\1/p" "${tmp}/SHA256SUMS")"
if [ "${ZONE_GH:-}" != 1 ]; then
  fetch "${name}" || fail "cannot download ${name}"
fi
[ -f "${tmp}/${name}" ] || fail "the release has no ${name}"

if [ -n "${ZONE_COSIGN_PUB:-}" ]; then
  cp "${ZONE_COSIGN_PUB}" "${tmp}/cosign.pub"
else
  printf '%s\n' "${release_key}" >"${tmp}/cosign.pub"
fi
verify="cosign verify-blob --key cosign.pub --bundle SHA256SUMS.cosign.bundle --insecure-ignore-tlog=true SHA256SUMS"
bad="SHA256SUMS.cosign.bundle is not a good signature of SHA256SUMS by the release key"
if [ ! -f "${tmp}/SHA256SUMS.cosign.bundle" ]; then
  [ "${ZONE_ALLOW_UNSIGNED:-}" = 1 ] ||
    fail "the release is not signed; set ZONE_ALLOW_UNSIGNED=1 to install it on its SHA-256 alone"
  echo "zone install: this release is UNSIGNED; only its SHA-256 is checked" >&2
elif command -v cosign >/dev/null 2>&1; then
  (cd "${tmp}" && ${verify} >/dev/null 2>&1) || fail "${bad}"
  echo "zone install: signature OK (cosign, release key)" >&2
elif command -v openssl >/dev/null 2>&1; then
  # The bundle's signature is ECDSA P-256 over SHA256SUMS itself, which openssl checks too.
  tr -d '\r\n ' <"${tmp}/SHA256SUMS.cosign.bundle" |
    sed -n 's/.*"signature":"\([A-Za-z0-9+/=]*\)".*/\1/p' | openssl base64 -d -A >"${tmp}/sig"
  openssl dgst -sha256 -verify "${tmp}/cosign.pub" -signature "${tmp}/sig" "${tmp}/SHA256SUMS" \
    >/dev/null 2>&1 || fail "${bad}"
  echo "zone install: signature OK (openssl, release key)" >&2
else
  echo "zone install: neither cosign nor openssl is installed, so the signature was not checked." >&2
  echo "  To check it, with cosign.pub from https://github.com/${repo}: ${verify}" >&2
fi

got="$(sha256 "${tmp}/${name}")"
[ "${got}" = "${want}" ] || fail "${name} does not match SHA256SUMS"
echo "zone install: ${name} SHA-256 OK" >&2

dir="${ZONE_INSTALL_DIR:-${HOME}/.local/bin}"
mkdir -p "${dir}"
chmod 0755 "${tmp}/${name}"
mv "${tmp}/${name}" "${dir}/zone"
echo "zone install: installed ${dir}/zone ($("${dir}/zone" --version))"
case ":${PATH}:" in
  *":${dir}:"*) ;;
  *) echo "zone install: add ${dir} to PATH" >&2 ;;
esac
echo "Next: zone join <invite>"
