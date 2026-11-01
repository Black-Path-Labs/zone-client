#!/usr/bin/env bash
# Builds a release `zone` binary for one target with the committed Cargo.lock and writes it to
# dist/zone-<version>-<target>. Linux targets are musl and fully static: the script fails if the
# result links anything dynamically. Run it on a maintainer's machine; this repo has no GitHub
# Actions.
#
#   scripts/build.sh x86_64-unknown-linux-musl
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

target="${1:-}"
case "${target}" in
  x86_64-unknown-linux-musl) ;;
  *)
    echo "usage: scripts/build.sh x86_64-unknown-linux-musl" >&2
    exit 2
    ;;
esac

if [ ! -d "$(rustc --print sysroot)/lib/rustlib/${target}" ]; then
  echo "build: the ${target} target is missing; run: rustup target add ${target}" >&2
  exit 1
fi

# ring compiles C for the target. musl-gcc is used when present; otherwise the host gcc builds
# those objects and the Rust toolchain's self-contained musl links them.
cc_var="CC_${target//-/_}"
if [ -z "${!cc_var:-}" ] && ! command -v musl-gcc >/dev/null; then
  export "${cc_var}=gcc"
fi

cargo build --release --locked --target "${target}" --bin zone

bin="target/${target}/release/zone"
kind="$(file -b "${bin}")"
if ! grep -Eq 'static(-pie)? linked' <<<"${kind}"; then
  echo "build: ${bin} is not statically linked: ${kind}" >&2
  exit 1
fi
libs="$(ldd "${bin}" 2>&1 || true)"
if ! grep -Eq 'not a dynamic|statically linked' <<<"${libs}"; then
  echo "build: ldd lists libraries for ${bin}" >&2
  exit 1
fi

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
mkdir -p dist
out="dist/zone-${version}-${target}"
cp "${bin}" "${out}"
echo "build: ${out} ($(stat -c %s "${out}") bytes)"
