#!/usr/bin/env bash
# Builds a release `zone` binary for one target with the committed Cargo.lock and writes it to
# dist/zone-<version>-<target>. Linux targets are musl and fully static: the script fails if the
# result links anything dynamically. Run it on a maintainer's machine; this repo has no GitHub
# Actions.
#
#   scripts/build.sh x86_64-unknown-linux-musl
#   scripts/build.sh aarch64-unknown-linux-musl   # needs zig and cargo-zigbuild on PATH
#   scripts/build.sh all-linux                    # both of the above
#
# scripts/cross-toolchain.sh fetches the pinned zig, cargo-zigbuild and qemu-user. With
# ZONE_QEMU_AARCH64 set (or qemu-aarch64 on PATH) the aarch64 binary must run `zone --version`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

target="${1:-}"
case "${target}" in
  x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
  all-linux)
    "$0" x86_64-unknown-linux-musl
    "$0" aarch64-unknown-linux-musl
    exit 0
    ;;
  *)
    echo "usage: scripts/build.sh x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|all-linux" >&2
    exit 2
    ;;
esac

if [ ! -d "$(rustc --print sysroot)/lib/rustlib/${target}" ]; then
  echo "build: the ${target} target is missing; run: rustup target add ${target}" >&2
  exit 1
fi

host="$(rustc -vV | sed -n 's/^host: //p')"
if [ "${target%%-*}" = "${host%%-*}" ]; then
  # ring compiles C for the target. musl-gcc is used when present; otherwise the host gcc builds
  # those objects and the Rust toolchain's self-contained musl links them.
  cc_var="CC_${target//-/_}"
  if [ -z "${!cc_var:-}" ] && ! command -v musl-gcc >/dev/null; then
    export "${cc_var}=gcc"
  fi
  build=(cargo build)
else
  # Another architecture: zig compiles ring's C and links, through cargo-zigbuild.
  for tool in zig cargo-zigbuild; do
    if ! command -v "${tool}" >/dev/null; then
      echo "build: ${tool} is not on PATH; see scripts/cross-toolchain.sh" >&2
      exit 1
    fi
  done
  build=(cargo zigbuild)
fi

# Build-machine paths (the cargo home, this checkout, the target dir) are rewritten in the panic
# locations the binary carries, so it names no maintainer's home and two checkouts build the same
# bytes. The target dir is inside the checkout, so its mapping comes last and wins.
target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p "${target_dir}"
target_dir="$(cd "${target_dir}" && pwd)"
remap="--remap-path-prefix=${CARGO_HOME:-${HOME}/.cargo}=/cargo"
remap+=" --remap-path-prefix=$(pwd)=/zone-client --remap-path-prefix=${target_dir}=/target"
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }${remap}"

"${build[@]}" --release --locked --target "${target}" --bin zone

bin="${target_dir}/${target}/release/zone"
kind="$(file -b "${bin}")"
if ! grep -Eq 'static(-pie|ally) linked' <<<"${kind}"; then
  echo "build: ${bin} is not statically linked: ${kind}" >&2
  exit 1
fi
libs="$(ldd "${bin}" 2>&1 || true)"
if ! grep -Eq 'not a dynamic|statically linked' <<<"${libs}"; then
  echo "build: ldd lists libraries for ${bin}" >&2
  exit 1
fi

if [ "${target}" = aarch64-unknown-linux-musl ]; then
  qemu="${ZONE_QEMU_AARCH64:-$(command -v qemu-aarch64 || true)}"
  if [ -n "${qemu}" ]; then
    "${qemu}" "${bin}" --version
  elif [ "$(uname -m)" = aarch64 ]; then
    "${bin}" --version
  else
    echo "build: no qemu-aarch64; ${bin} was not run" >&2
  fi
fi

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
mkdir -p dist
out="dist/zone-${version}-${target}"
cp "${bin}" "${out}"
echo "build: ${out} ($(stat -c %s "${out}") bytes)"
