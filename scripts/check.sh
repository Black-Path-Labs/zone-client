#!/usr/bin/env bash
# The zone client gate: rustfmt, clippy with warnings as errors, and every test, all with the
# committed Cargo.lock. It runs on a maintainer's machine. This repo has no GitHub Actions, and
# the gate fails if a workflow appears.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [ -e .github/workflows ]; then
  echo "check: .github/workflows must not exist; this repo runs no GitHub Actions" >&2
  exit 1
fi
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
