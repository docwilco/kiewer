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
# The viewer for the browser: the wasm32 build of kiewer_viewer (and so
# of kicad_core) through scripts/build-viewer.sh, which also runs
# wasm-bindgen over it, so the pinned tool is proved to read what the
# crate wrote. The debug profile, into the target directory: the release
# output under kiewer_viewer/dist is what the server embeds, and a check
# never overwrites it. The target and the tool come from rustup and cargo
# install (README, Development; the CI workflow's wasm job); the script
# names what is missing.
#
# Clippy after the build, so a missing target or tool is still named by
# scripts/build-viewer.sh: code behind cfg(target_arch = "wasm32") is
# linted here and nowhere else. The same package and the same default
# target directory as the build, so the two share build scripts and proc
# macros. Cargo keeps check and build units apart, so the first run on a
# target directory checks each wasm32 dependency once; after that a run
# checks only what changed. The -p is the exception
# scripts/build-viewer.sh documents.
stage_wasm() {
  local target_dir
  target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
  scripts/build-viewer.sh debug "$target_dir/viewer-check"
  cargo clippy -p kiewer_viewer --lib --target wasm32-unknown-unknown -- -D warnings
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
