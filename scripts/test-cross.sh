#!/usr/bin/env bash
# Runs the `zone` unit tests built for aarch64 musl under qemu-user on an x86_64 host. The
# integration tests spawn the binary themselves, so they stay with scripts/check.sh on the host.
# Needs zig and cargo-zigbuild on PATH and ZONE_QEMU_AARCH64 (or qemu-aarch64 on PATH); see
# scripts/cross-toolchain.sh.
#
#   scripts/test-cross.sh aarch64-unknown-linux-musl
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

target="${1:-}"
if [ "${target}" != aarch64-unknown-linux-musl ]; then
  echo "usage: scripts/test-cross.sh aarch64-unknown-linux-musl" >&2
  exit 2
fi
qemu="${ZONE_QEMU_AARCH64:-$(command -v qemu-aarch64 || true)}"
if [ -z "${qemu}" ]; then
  echo "test-cross: no qemu-aarch64; see scripts/cross-toolchain.sh" >&2
  exit 1
fi

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER="${qemu}"
cargo zigbuild test --locked --target "${target}" --bin zone
