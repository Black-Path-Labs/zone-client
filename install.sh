#!/bin/sh
# Installs the `zone` client: downloads the release binary for this OS and CPU with its
# SHA256SUMS, checks the sum (and the minisign signature when minisign is installed) and puts it
# in ~/.local/bin/zone. It never uses sudo.
#
#   curl -fsSL https://github.com/Black-Path-Labs/zone-client/releases/latest/download/install.sh | sh
#   curl -fsSL .../install.sh | ZONE_VERSION=v0.1.0 sh      # a pinned release
#
# ZONE_INSTALL_DIR replaces ~/.local/bin. ZONE_GH=1 downloads with `gh release download`, which
# also reaches a private repository or a draft release with gh's login. ZONE_PUBKEY replaces the
# release public key below. Once a key is set, an unsigned release is refused unless
# ZONE_ALLOW_UNSIGNED=1.
set -eu

repo="Black-Path-Labs/zone-client"
# The release public key. None is published yet, so releases are UNSIGNED and only the sums
# can be checked.
pubkey="${ZONE_PUBKEY:-}"

fail() {
  echo "zone install: $*" >&2
  exit 1
}

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
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
    -p SHA256SUMS -p 'SHA256SUMS.minisig' -p "zone-*-${target}" ||
    fail "gh release download failed"
else
  fetch SHA256SUMS || fail "cannot download SHA256SUMS"
  fetch SHA256SUMS.minisig 2>/dev/null || rm -f "${tmp}/SHA256SUMS.minisig"
fi

name="$(sed -n "s/^[0-9a-f]\{64\}  \(zone-[^ ]*-${target}\)\$/\1/p" "${tmp}/SHA256SUMS" | head -1)"
[ -n "${name}" ] || fail "the release has no zone binary for ${target}"
want="$(sed -n "s/^\([0-9a-f]\{64\}\)  ${name}\$/\1/p" "${tmp}/SHA256SUMS")"
if [ "${ZONE_GH:-}" != 1 ]; then
  fetch "${name}" || fail "cannot download ${name}"
fi
[ -f "${tmp}/${name}" ] || fail "the release has no ${name}"

if [ ! -f "${tmp}/SHA256SUMS.minisig" ] && [ -n "${pubkey}" ] &&
  [ "${ZONE_ALLOW_UNSIGNED:-}" != 1 ]; then
  fail "the release is not signed; set ZONE_ALLOW_UNSIGNED=1 to install it on its SHA-256 alone"
elif [ -f "${tmp}/SHA256SUMS.minisig" ] && [ -n "${pubkey}" ]; then
  if command -v minisign >/dev/null 2>&1; then
    minisign -Vq -P "${pubkey}" -m "${tmp}/SHA256SUMS" -x "${tmp}/SHA256SUMS.minisig" ||
      fail "SHA256SUMS.minisig is not a good signature of SHA256SUMS"
    echo "zone install: signature OK" >&2
  else
    echo "zone install: minisign is not installed, so the signature was not checked. To check it:" >&2
    echo "  minisign -Vm SHA256SUMS -P ${pubkey}" >&2
  fi
elif [ -f "${tmp}/SHA256SUMS.minisig" ]; then
  echo "zone install: no release public key is published yet; only the SHA-256 is checked" >&2
else
  echo "zone install: this release is UNSIGNED; only its SHA-256 is checked" >&2
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
