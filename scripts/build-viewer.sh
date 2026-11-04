#!/usr/bin/env bash
# The viewer for the browser: kiewer_viewer compiled to wasm32, then
# wasm-bindgen's JavaScript glue around it. The output directory is what
# kiewer_lib embeds (its build.rs): kiewer_viewer/dist, git-ignored, so
# the dev loop is this script once and `cargo run` after.
#
# wasm-bindgen-cli must be the version Cargo.lock names for the
# wasm-bindgen crate (scripts/wasm-bindgen-version.sh); the tool refuses
# a wasm built against another, so the mismatch is named here with the
# install line, before the build runs.
#
# Usage: scripts/build-viewer.sh [release|debug|probe] [out-dir]
#   default: release into kiewer_viewer/dist. scripts/checks.sh wasm runs
#   the debug profile into the target directory, so a check never
#   overwrites what the server embeds. probe is the debug profile with
#   kiewer_viewer's panic-probe feature, the viewer the browser stage
#   tests (#94); the crate refuses that feature in a release build.
set -euo pipefail
cd "$(dirname "$0")/.."

profile="${1:-release}"
out_dir="${2:-kiewer_viewer/dist}"
case "$profile" in
  release) profile_flag=(--release); profile_dir=release ;;
  debug) profile_flag=(); profile_dir=debug ;;
  probe) profile_flag=(--features panic-probe); profile_dir=debug ;;
  *) echo "usage: $0 [release|debug|probe] [out-dir]" >&2; exit 2 ;;
esac

rustup target list --installed | grep -qx wasm32-unknown-unknown \
  || { echo "Run 'rustup target add wasm32-unknown-unknown' first." >&2; exit 1; }

wanted="$(scripts/wasm-bindgen-version.sh)"
have="$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)"
if [ "$have" != "$wanted" ]; then
  echo "wasm-bindgen-cli $wanted is needed (found '${have:-none}'); run:" >&2
  echo "  cargo install wasm-bindgen-cli --version $wanted --locked" >&2
  exit 1
fi

target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
# A named package, not the workspace: the server does not build for
# wasm32. The feature-unification concern behind "never -p"
# (rust-conventions) is about duplicate artifacts under one target
# directory; a --target build has a directory of its own. Within it,
# debug and probe are two feature sets of kiewer_viewer alone, each kept
# under deps, and both write the one debug/kiewer_viewer.wasm, which
# wasm-bindgen reads right after, so each out-dir gets its own build.
cargo build --target wasm32-unknown-unknown -p kiewer_viewer --lib "${profile_flag[@]}"
mkdir -p "$out_dir"
# --target web: an ES module with a default export that fetches the wasm
# beside it, which the viewer page imports; no TypeScript, nothing reads it.
wasm-bindgen --target web --no-typescript --out-dir "$out_dir" \
  "$target_dir/wasm32-unknown-unknown/$profile_dir/kiewer_viewer.wasm"
