#!/usr/bin/env bash
# Prints the sqlx version Cargo.lock resolved, which is the version of
# sqlx-cli to install: the two are released together, and the offline
# data in .sqlx/ that `cargo sqlx prepare` writes is what the crate's
# query macros read. Every place that installs the tool (README
# Development, the CI workflow) reads it from here, so a `cargo update`
# that moves the crate moves the tool with it; scripts/sqlx-prepare.sh
# refuses any other version.
#
# Usage: scripts/sqlx-version.sh
set -euo pipefail
cd "$(dirname "$0")/.."
cargo metadata --format-version 1 --locked \
  | jq -r '.packages[] | select(.name == "sqlx") | .version'
