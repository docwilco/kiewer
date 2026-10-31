#!/usr/bin/env bash
# Prints the wasm-bindgen version Cargo.lock resolved, which is the one
# version of wasm-bindgen-cli that can read the viewer's wasm: the crate
# and the tool share a format and the tool refuses any other version.
# Every place that installs the tool (README Development, the CI
# workflow) reads it from here, so a `cargo update` that moves the crate
# moves the tool with it.
#
# Usage: scripts/wasm-bindgen-version.sh
set -euo pipefail
cd "$(dirname "$0")/.."
cargo metadata --format-version 1 --locked \
  | jq -r '.packages[] | select(.name == "wasm-bindgen") | .version'
