#!/usr/bin/env bash
# Refresh (or, with --check, verify) the committed sqlx offline data in
# .sqlx/: every statement in kiewer_lib is a query macro, which compiles
# against that data on any checkout with no database at hand
# (scripts/checks.sh exports SQLX_OFFLINE=true for every stage), and the
# sqlx stage of scripts/checks.sh runs this with --check, failing when
# the data no longer matches the statements or the schema.
#
# The schema the statements are checked against is exactly what
# migrations/ makes: a scratch database beside the one DATABASE_URL names,
# that name plus `_sqlx` (kiewer_sqlx against the compose Postgres and
# on CI), is dropped, created and migrated, then `cargo sqlx prepare`
# compiles every target of every crate against it. One run at a time per
# DATABASE_URL, since each run starts by dropping the scratch database.
# After a run the scratch database is left migrated: the database to
# point DATABASE_URL at, with SQLX_OFFLINE=false, while a statement is in
# flux (README, Development).
#
# Needs DATABASE_URL on a Postgres whose role may create databases
# (compose's and CI's `kiewer` is the image's superuser), sqlx-cli at the
# version Cargo.lock pins, and jq (for scripts/sqlx-version.sh):
#
#     cargo install sqlx-cli --version "$(scripts/sqlx-version.sh)" --locked \
#       --no-default-features --features postgres,rustls
#
# Usage: scripts/sqlx-prepare.sh [--check]
set -euo pipefail
cd "$(dirname "$0")/.."

install='cargo install sqlx-cli --version "$(scripts/sqlx-version.sh)" --locked --no-default-features --features postgres,rustls'

[ -n "${DATABASE_URL:-}" ] || {
  echo "sqlx-prepare: DATABASE_URL is unset or empty; it names the Postgres the scratch database goes on (README, Development)." >&2
  exit 1
}

command -v sqlx >/dev/null || {
  echo "sqlx-prepare: sqlx-cli is required:" >&2
  echo "    $install" >&2
  exit 1
}

# The data's format is the tool's, so a tool that is not the crate's
# version could write data the macros misread, or call fresh data stale.
pinned="$(scripts/sqlx-version.sh)"
installed="$(sqlx --version | awk '{print $2}')"
[ "$installed" = "$pinned" ] || {
  echo "sqlx-prepare: sqlx-cli is $installed; Cargo.lock's sqlx is $pinned:" >&2
  echo "    $install" >&2
  exit 1
}

# The URL with `_sqlx` after its database name, its query string kept.
base="${DATABASE_URL%%\?*}"
query="${DATABASE_URL#"$base"}"
name="${base##*/}"
case "$base" in
  postgres://*/*|postgresql://*/*) ;;
  *) name="" ;;
esac
[ -n "$name" ] || {
  echo "sqlx-prepare: DATABASE_URL names no database: $base" >&2
  exit 1
}
export DATABASE_URL="${base%/*}/${name}_sqlx$query"

sqlx database reset -y --source migrations
# The arguments after `--` go to the `cargo check` sqlx-cli runs, with
# SQLX_OFFLINE=false and DATABASE_URL set for it: --workspace reaches
# every crate whatever the default members are, and --all-targets would
# put a test's macro in .sqlx/ too (the tests use none today, so the data
# is the library's statements alone, one file each).
cargo sqlx prepare --workspace "$@" -- --workspace --all-targets
