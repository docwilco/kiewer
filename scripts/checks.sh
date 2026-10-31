#!/usr/bin/env bash
# Single source of truth for this repo's checks. The pre-commit hook
# (.githooks/pre-commit) and CI (.github/workflows/ci.yml) both run this
# script, so what passes locally is exactly what passes in CI.
#
# Usage: scripts/checks.sh [fmt|clippy|test|docs|wasm|all]   (default: all)
set -euo pipefail
cd "$(dirname "$0")/.."

stage_fmt() {
  cargo fmt --all --check || { echo "Run 'cargo fmt --all' to fix." >&2; return 1; }
}
stage_clippy() { cargo clippy --workspace --all-targets -- -D warnings; }
# The workspace build first: cargo builds a package's binaries for its own
# integration tests only, so a test that spawns another package's binary
# would otherwise run whatever the last build left. With --all-targets, so
# the build resolves as `cargo test` does: a command that names a test
# target adds the dev-dependencies, and resolver 3 unifies features over
# the whole command, so a plain `cargo build` gives shared crates another
# feature set, another hash and a second compile under `cargo test`.
stage_test()   { cargo build --workspace --all-targets && cargo test --workspace; }
stage_docs()   { RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps; }
# The viewer for the browser. The one package by name, not the workspace:
# the server does not build for wasm32. The feature-unification concern
# behind "never -p" (rust-conventions) is about duplicate artifacts under
# one target directory; a --target build has a directory of its own. The
# target comes from rustup (README, Development; the CI workflow's wasm
# job), so a missing one is named rather than left to cargo's error.
stage_wasm() {
  rustup target list --installed | grep -qx wasm32-unknown-unknown \
    || { echo "Run 'rustup target add wasm32-unknown-unknown' first." >&2; return 1; }
  cargo build --target wasm32-unknown-unknown -p kiewer_viewer --lib
}

run() {
  local name=$1 start
  echo "==> $name"
  start=$(date +%s)
  "stage_$name"
  echo "==> $name ok ($(( $(date +%s) - start ))s)"
}

case "${1:-all}" in
  fmt|clippy|test|docs|wasm) run "$1" ;;
  all) for stage in fmt clippy test docs wasm; do run "$stage"; done ;;
  *) echo "usage: $0 [fmt|clippy|test|docs|wasm|all]" >&2; exit 2 ;;
esac
