#!/usr/bin/env bash
# Single source of truth for this repo's checks. The pre-commit hook
# (.githooks/pre-commit) and CI (.github/workflows/ci.yml) both run this
# script, so what passes locally is exactly what passes in CI.
#
# Usage: scripts/checks.sh [fmt|sqlx|clippy|test|docs|wasm|browser|all]   (default: all)
#
# test and all need DATABASE_URL and KIEWER_GCS_EMULATOR, or their opt-outs
# KIEWER_SKIP_DB_TESTS=1 and KIEWER_SKIP_GCS_TESTS=1 (the gates, below);
# sqlx needs DATABASE_URL or KIEWER_SKIP_DB_TESTS=1.
set -euo pipefail
cd "$(dirname "$0")/.."

# Every build here compiles the query macros against the offline data
# committed in .sqlx/, never against a database: DATABASE_URL is set for
# the tests, on a database they migrate only when they run, and sqlx's
# macros would otherwise read it (or a .env's, which they look for in
# this directory and every one above it) and connect at compile time.
# The sqlx stage is where the data meets a database: `cargo sqlx prepare`
# sets SQLX_OFFLINE=false for its own build.
export SQLX_OFFLINE=true

stage_fmt() {
  cargo fmt --all --check || { echo "Run 'cargo fmt --all' to fix." >&2; return 1; }
}
# The offline data in .sqlx/ against the statements and against the
# schema migrations/ makes, on a scratch database beside DATABASE_URL's
# (scripts/sqlx-prepare.sh, whose header has the install line for
# sqlx-cli). Stale data fails here by name, before clippy's build fails
# on a statement with no data. Without DATABASE_URL, which the gate lets
# through only under KIEWER_SKIP_DB_TESTS=1, a skip with a line saying so.
stage_sqlx() {
  if [ -z "${DATABASE_URL:-}" ]; then
    echo "sqlx: skipped, DATABASE_URL is unset (KIEWER_SKIP_DB_TESTS=1)"
    return 0
  fi
  scripts/sqlx-prepare.sh --check
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
# scripts/build-viewer.sh documents. A second clippy with panic-probe,
# so the browser tests' panic in Viewer.create is linted too.
stage_wasm() {
  local target_dir
  target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
  scripts/build-viewer.sh debug "$target_dir/viewer-check"
  cargo clippy -p kiewer_viewer --lib --target wasm32-unknown-unknown -- -D warnings
  cargo clippy -p kiewer_viewer --lib --target wasm32-unknown-unknown --features panic-probe -- -D warnings
}

# The board and schematic pages in a real browser
# (kiewer_lib/tests/it/browser.rs): chrome-headless-shell at the version
# scripts/browser.sh pins, drawing WebGL2 on SwiftShader, against a
# server built with the viewer in it. The viewer is the debug build with
# panic-probe on (scripts/build-viewer.sh probe), so a test can make a
# real panic inside Viewer.create: beside the wasm stage's build in the
# target directory, which costs one more compile of kiewer_viewer alone.
# It is named to kiewer_lib/build.rs by KIEWER_VIEWER_DIST: the snapshot
# and CI have no kiewer_viewer/dist, and a check never writes there.
#
# The variable changes kiewer_lib's build script output, so kiewer_lib
# and what links it compile once here and once more in the next test
# stage, in the same target directory: a compile each way, where a
# directory of its own would hold a second copy of every dependency.
# No workspace build first, unlike the test stage: the test spawns no
# binary of the workspace, and a build would relink every binary with
# the variable set, for nothing.
#
# No browser is a skip with a line saying so; on CI (which sets CI) it
# is a failure, since the workflow installs it and a skip would hide a
# broken install.
stage_browser() {
  local browser target_dir
  browser="$(scripts/browser.sh path)"
  if [ ! -x "$browser" ]; then
    if [ -n "${CI:-}" ]; then
      echo "no chrome-headless-shell at $browser; the workflow installs it with scripts/browser.sh install" >&2
      return 1
    fi
    echo "browser: skipped, no chrome-headless-shell at $browser (scripts/browser.sh install; README, Development)"
    return 0
  fi
  target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
  scripts/build-viewer.sh probe "$target_dir/viewer-probe"
  KIEWER_VIEWER_DIST="$target_dir/viewer-probe" KIEWER_BROWSER="$browser" \
    cargo test --workspace --test it browser::
}

# The test stage's gates (#68). The tests on Postgres and on the GCS
# emulator return early without their variable and count as passed, so a
# run without one would say green having checked none of them. Without
# the variable (unset or empty) the run fails, before any stage, unless
# the gate's opt-out is set to 1; an opt-out prints a loud line naming
# what skips, once here and once more after the last stage, where a green
# run ends. CI sets both variables and refuses both opt-outs, so a
# workflow that lost one fails instead of skipping.
skipped=()
gate() {  # $1: the variable, $2: its opt-out, $3: the service, $4: a function naming the tests on it
  local var=$1 opt_out=$2 service=$3 tests
  [ -z "${!var:-}" ] || return 0
  tests=$("$4")
  if [ "${!opt_out:-}" != 1 ]; then
    echo "test: $var is unset or empty, so the tests on $service would skip and count as passed ($tests)." >&2
    echo "  Export $var (README, Development), or set $opt_out=1 to run without them." >&2
    return 1
  fi
  if [ -n "${CI:-}" ]; then
    echo "test: $opt_out=1 is refused on CI, which sets $var: a skip would hide a broken workflow." >&2
    return 1
  fi
  skipped+=("!!! test: $opt_out=1, so the tests on $service skip and count as passed: $tests")
}
# The files that take a pool from kiewer_test_support::db; every user of
# it is under kiewer_lib/tests/it/ today.
db_tests() {
  local file names=()
  for file in kiewer_lib/tests/it/*.rs; do
    if grep -qE 'pool_from_env|pool_of_its_own' "$file"; then names+=("${file##*/}"); fi
  done
  echo "kiewer_lib/tests/it/: ${names[*]:-none found}; and the sqlx stage"
}
gcs_tests() { echo kiewer_lib/tests/it/gcs_blob_store.rs; }
gates() {
  local status=0
  gate DATABASE_URL KIEWER_SKIP_DB_TESTS Postgres db_tests || status=1
  gate KIEWER_GCS_EMULATOR KIEWER_SKIP_GCS_TESTS "the GCS emulator" gcs_tests || status=1
  [ "$status" = 0 ] || exit 1
  report_skipped
}
report_skipped() {
  [ "${#skipped[@]}" = 0 ] || printf '%s\n' "${skipped[@]}" >&2
}

run() {
  local name=$1 start
  echo "==> $name"
  start=$(date +%s)
  "stage_$name"
  echo "==> $name ok ($(( $(date +%s) - start ))s)"
}

case "${1:-all}" in
  test) gates; run test; report_skipped ;;
  sqlx) gate DATABASE_URL KIEWER_SKIP_DB_TESTS Postgres db_tests || exit 1; run sqlx; report_skipped ;;
  fmt|clippy|docs|wasm|browser) run "$1" ;;
  all) gates; for stage in fmt sqlx clippy test docs wasm browser; do run "$stage"; done; report_skipped ;;
  *) echo "usage: $0 [fmt|sqlx|clippy|test|docs|wasm|browser|all]" >&2; exit 2 ;;
esac
