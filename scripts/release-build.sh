#!/usr/bin/env bash
# The one spelling of the release build: the binaries the image copies,
# compiled on the runner (or a dev box), never inside the Dockerfile. The
# CI workflow's `release-build` job runs it on every push and pull
# request under the `release` cargo cache (the dependencies warm) and
# hands the result to the image job as a workflow artifact. Not a stage
# of scripts/checks.sh: the hook never runs it, since a release profile
# build is minutes the checks do not need.
#
# KIEWER_BUILD is the short commit hash the executable logs on its
# first line (release-conventions); the default is the hash of HEAD.
# rustc records the variable and its value in the dep-info, so a change
# of it rebuilds the crate that reads it and all that depends on it.
#
# Usage: scripts/release-build.sh
#   (then `docker build -t kiewer .`; a dev box runs the same two)
set -euo pipefail
cd "$(dirname "$0")/.."

export KIEWER_BUILD="${KIEWER_BUILD:-$(git rev-parse --short HEAD)}"

echo "release-build: KIEWER_BUILD=$KIEWER_BUILD"
cargo build --release --locked -p kiewer
