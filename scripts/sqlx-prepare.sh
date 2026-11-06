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
# compiles every target of every crate against it. Runs on one scratch
# database take turns (the flock below). After a run the scratch database is left migrated: the database to
# point DATABASE_URL at, with SQLX_OFFLINE=false, while a statement is in
# flux (README, Development).
#
# Needs DATABASE_URL on a Postgres whose role may create databases
# (compose's and CI's `kiewer` is the image's superuser), flock, jq (for
# scripts/sqlx-version.sh) and sqlx-cli at the version Cargo.lock pins:
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

command -v jq >/dev/null || {
  echo "sqlx-prepare: jq is required (apt-get install jq)." >&2
  exit 1
}

command -v sqlx >/dev/null && command -v cargo-sqlx >/dev/null || {
  echo "sqlx-prepare: sqlx-cli is required:" >&2
  echo "    $install" >&2
  exit 1
}

# The data's format is the tool's, so a tool that is not the crate's
# version could write data the macros misread, or call fresh data stale.
# sqlx-cli installs two binaries, and this script runs both: `sqlx` for
# the database, `cargo-sqlx` (as `cargo sqlx`) for the prepare, which is
# what writes and checks the data. Each is checked, since a binary
# replaced by hand leaves the other behind (on 2026-11-05 the prepare ran
# 0.8.6 against sqlx 0.9 under a guard that read `sqlx` alone).
pinned="$(scripts/sqlx-version.sh)"
for tool in "sqlx" "cargo sqlx"; do
  installed="$($tool --version | awk '{print $2}')"
  [ "$installed" = "$pinned" ] || {
    echo "sqlx-prepare: \`$tool\` is sqlx-cli $installed; Cargo.lock's sqlx is $pinned:" >&2
    echo "    $install" >&2
    exit 1
  }
done

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

# One run at a time per scratch database: each starts by dropping it, so
# a second run (a commit's hook beside a hand run, or two worktrees on one
# DATABASE_URL) waits here rather than pulling it out from under the first.
exec 9>"${TMPDIR:-/tmp}/kiewer-sqlx-prepare-${name}_sqlx.lock"
flock 9

# `cargo sqlx prepare` sets the mtime of every target's source file of
# every workspace member that depends on sqlx-macros to now
# (minimal_project_recompile_action in sqlx-cli's src/prepare.rs; the list
# is targets[].src_path, so the lib and bin roots and every integration
# test), so that its own `cargo check` reruns the query macros. Cargo marks
# a unit stale when a source in its dep-info is newer than the unit's
# fingerprint, so every unit built before this script ran would be dirty
# at the next build: under `scripts/checks.sh all` the clippy, test and
# browser stages after this one compiled kiewer_lib again on every commit,
# whatever it changed. So the mtimes are saved before the prepare and put
# back after it.
#
# The files sqlx-cli will touch, from the same answer it reads them from:
# every target's source file of every workspace member, so a new test file
# is in the list without a change here; an empty list means jq failed
# inside the substitution, where set -e cannot see it, so it is refused
# rather than run with nothing saved. A copy of each source keeps its
# bytes and its mtime (cp -p), and the mtime goes back with `touch -r`, at
# full precision and with no date string parsed, only to a source whose
# bytes are still the copy's: one edited or checked out while this ran
# keeps its new mtime, or cargo would call its stale unit fresh. The
# sources are the targets' roots, a handful of files. The restore is an
# EXIT trap, so a failed
# --check (the hook's normal red), an error below or an interrupt restores
# too, and the script's exit status stays the prepare's: set -e is off
# inside the trap, since under it one failed touch would end the restore
# early and make the status its own; -c on the way back skips a source
# removed meanwhile instead of recreating it empty. sqlx's own check units
# are fingerprinted after the touch, so under the restored mtime they are
# fresh as well; the next prepare touches again, so the query macros still
# rerun for the check. The saved mtimes live under the target directory,
# which only cargo can name (a config file may move it).
metadata="$(cargo metadata --format-version 1 --no-deps)"
target_dir="$(jq -r .target_directory <<<"$metadata")"
mapfile -t sources < <(jq -r '.packages[].targets[].src_path' <<<"$metadata")
[ "${#sources[@]}" -gt 0 ] || {
  echo "sqlx-prepare: cargo metadata named no target source file" >&2
  exit 1
}
mkdir -p "$target_dir/sqlx"
saved_mtimes="$(mktemp -d "$target_dir/sqlx/saved_mtimes.XXXXXX")"
trap 'rm -rf "$saved_mtimes"' EXIT
restore_mtimes() {
  local i
  set +e
  for i in "${!sources[@]}"; do
    cmp -s "$saved_mtimes/$i" "${sources[$i]}" &&
      touch -c -r "$saved_mtimes/$i" "${sources[$i]}"
  done
  rm -rf "$saved_mtimes"
}
for i in "${!sources[@]}"; do cp -p "${sources[$i]}" "$saved_mtimes/$i"; done
trap restore_mtimes EXIT

# 9>&- on every command that starts others: a daemon one of them starts
# (sccache's server, started by the rustc wrapper when none runs) would
# otherwise inherit the lock and hold it past this script's exit, until its
# own idle timeout, and the next prepare on this database would wait for it.
sqlx database reset -y --source migrations 9>&-
# The arguments after `--` go to the `cargo check` sqlx-cli runs, with
# SQLX_OFFLINE=false and DATABASE_URL set for it: --workspace reaches
# every crate whatever the default members are, and --all-targets would
# put a test's macro in .sqlx/ too (the tests use none today, so the data
# is the library's statements alone, one file each).
#
# --target-dir gives that check a target directory of its own under the
# workspace's. The touch above reaches kiewer_lib/build.rs too (a build
# script is a target), and the check runs before the mtimes come back, so
# in the shared target it reran the build script; a rerun counts as rebuilt
# whatever it wrote, and the next build of kiewer_lib (the test stage's,
# the browser stage's) then compiled again, with everything that links it.
# In a directory of its own the check reruns it there, and the shared
# target, under the restored mtimes, stays fresh. The price is a second set
# of check artifacts for the dependencies, from sccache where it has them,
# and more than once: the pre-commit hook's sweep removes what a commit
# did not use, so a commit that skips the sqlx stage leaves this directory
# swept, and the next one that runs it checks the dependencies again. A
# flag and not CARGO_TARGET_DIR, which sccache would put in every
# compile's cache key.
cargo sqlx prepare --workspace "$@" -- --workspace --all-targets \
  --target-dir "$target_dir/sqlx/check" 9>&-
