#!/usr/bin/env bash
# Fetches the pinned cross-compile toolchain into a directory and checks every download against
# the SHA-256 recorded here: zig and cargo-zigbuild link the musl targets that are not the host,
# and qemu-user runs the aarch64 binary and its unit tests on an x86_64 Linux host. Already
# fetched files are re-checked, not re-downloaded. Prints the env lines scripts/build.sh reads.
#
#   scripts/cross-toolchain.sh <dir>
set -euo pipefail

dir="${1:-}"
if [ -z "${dir}" ]; then
  echo "usage: scripts/cross-toolchain.sh <dir>" >&2
  exit 2
fi
if [ "$(uname -sm)" != "Linux x86_64" ]; then
  echo "cross-toolchain: pins exist for an x86_64 Linux host only" >&2
  exit 1
fi

zig_version=0.17.0
zig_tar="zig-x86_64-linux-${zig_version}.tar.xz"
zig_url="https://ziglang.org/download/${zig_version}/${zig_tar}"
zig_sha=1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026

zigbuild_version=0.24.0
zigbuild_tar="cargo-zigbuild-x86_64-unknown-linux-musl.tar.xz"
zigbuild_url="https://github.com/rust-cross/cargo-zigbuild/releases/download/v${zigbuild_version}/${zigbuild_tar}"
zigbuild_sha=52be2cb85ad3875682564a29bac1832b2ab3db68b1c504765c92d5249cc9b96f

# Ubuntu's qemu-user package; its emulators are statically linked, so no install is needed. The
# archive drops superseded versions: if this URL 404s, pin the current one and its SHA-256.
qemu_version=11.0.3
qemu_deb="qemu-user_${qemu_version}+ds-2ubuntu2_amd64.deb"
qemu_url="http://archive.ubuntu.com/ubuntu/pool/universe/q/qemu/qemu-user_${qemu_version}%2bds-2ubuntu2_amd64.deb"
qemu_sha=5e54f8ae25e85292d0c398ff06ba16fbdb2413b79c8846bf3fcb61acc6208ec8

mkdir -p "${dir}/dl"
dir="$(cd "${dir}" && pwd)"

fetch() {
  local url="$1" file="$2" sha="$3"
  if [ ! -f "${dir}/dl/${file}" ]; then
    curl -fsSL -o "${dir}/dl/${file}.part" "${url}"
    mv "${dir}/dl/${file}.part" "${dir}/dl/${file}"
  fi
  if ! echo "${sha}  ${dir}/dl/${file}" | sha256sum -c --quiet; then
    echo "cross-toolchain: ${file} does not match its pinned SHA-256" >&2
    exit 1
  fi
}

fetch "${zig_url}" "${zig_tar}" "${zig_sha}"
fetch "${zigbuild_url}" "${zigbuild_tar}" "${zigbuild_sha}"
fetch "${qemu_url}" "${qemu_deb}" "${qemu_sha}"

zig_dir="${dir}/zig-x86_64-linux-${zig_version}"
[ -x "${zig_dir}/zig" ] || tar -C "${dir}" -xf "${dir}/dl/${zig_tar}"
zigbuild_dir="${dir}/cargo-zigbuild-${zigbuild_version}"
if [ ! -x "${zigbuild_dir}/cargo-zigbuild-x86_64-unknown-linux-musl/cargo-zigbuild" ]; then
  mkdir -p "${zigbuild_dir}"
  tar -C "${zigbuild_dir}" -xf "${dir}/dl/${zigbuild_tar}"
fi
qemu_dir="${dir}/qemu-user-${qemu_version}"
[ -x "${qemu_dir}/usr/bin/qemu-aarch64" ] || dpkg-deb -x "${dir}/dl/${qemu_deb}" "${qemu_dir}"

echo "export PATH=\"${zig_dir}:${zigbuild_dir}/cargo-zigbuild-x86_64-unknown-linux-musl:\${PATH}\""
echo "export ZONE_QEMU_AARCH64=\"${qemu_dir}/usr/bin/qemu-aarch64\""
