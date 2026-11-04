# Sourced by pre-commit and post-merge; not a hook name, so git never runs
# it on its own.
#
# cargo-sweep keeps a build directory from growing without bound. Cargo
# never removes an artifact: any change to a dependency list or a feature set
# gives the same crate a new hash and the old files stay (one manifest change
# left a workspace 12 GiB of stale test binaries). A checkout's own target is
# swept by age: the last read of a unit's fingerprint, which relatime
# refreshes at most once a day, so three days keeps everything built or
# checked daily and drops the rest. A release build a daemon runs from goes
# cold after three unused days. Age and not use, because nothing brackets
# the builds run in a checkout the way a hook brackets its own checks.
#
# rustc's incremental cache is outside cargo-sweep's reach: one directory per
# crate id, which is a hash of the same metadata, so every cause of a stale
# artifact in deps leaves an orphaned directory of object files here, and
# neither rustc nor cargo ever removes one. A fresh unit never opens its
# directory, so use cannot be seen; the directory's mtime is its last
# compile, and one not compiled in three days belongs to a hash that will
# not be compiled again. Every profile's directory is swept, a target
# triple's (wasm32-unknown-unknown/release) and a nested target
# directory's (rust-analyzer's analyzer/debug) included.

sweep_incremental() {  # $1: the hook's name, $2: a target directory
  local old
  old=$(find "$2" -mindepth 2 -maxdepth 4 -type d -name incremental \
    -exec find {} -mindepth 1 -maxdepth 1 -type d -mtime +3 \; 2>/dev/null || true)
  [ -n "$old" ] || return 0
  echo "$1: removing $(printf '%s\n' "$old" | wc -l) incremental" \
    "directories not compiled in three days under $2"
  printf '%s\n' "$old" | xargs -d '\n' rm -rf --
}

# The checkout's own target, by age, wherever cargo puts it
# (CARGO_TARGET_DIR, a config's build.target-dir). Every profile lock under
# it is taken and held for the whole sweep, so a build that starts meanwhile
# waits for the sweep instead of reading a directory being emptied, and a
# build already holding one skips the sweep. Skipped too when cargo-sweep is
# missing, when the target does not exist yet, and when a lock cannot be
# opened (another user's file); a line says which.
sweep_checkout() {  # $1: the hook's name, $2: the checkout's root
  local target lock fd status
  target=$(cd "$2" && cargo metadata --format-version 1 --no-deps 2>/dev/null |
    jq -r .target_directory) || target=""
  if [ -z "$target" ] || [ ! -d "$target" ]; then
    echo "$1: no target directory yet; no sweep."
    return 0
  fi
  if ! command -v cargo-sweep >/dev/null; then
    echo "$1: cargo-sweep is missing (cargo install cargo-sweep --locked); no sweep."
    return 0
  fi
  local fds=()
  while IFS= read -r lock; do
    if ! exec {fd}<"$lock"; then
      echo "$1: cannot open $lock; no sweep."
      return 0
    fi
    fds+=("$fd")
    status=0
    flock -n "$fd" || status=$?
    if [ "$status" != 0 ]; then
      echo "$1: a build holds $lock; the checkout's sweep is skipped."
      for fd in "${fds[@]}"; do exec {fd}<&-; done
      return 0
    fi
  done < <(find "$target" -maxdepth 4 -name .cargo-lock 2>/dev/null)
  sweep_incremental "$1" "$target"
  (cd "$2" && cargo sweep --time 3 .) || echo "$1: cargo sweep failed; the sweep is incomplete."
  for fd in "${fds[@]}"; do exec {fd}<&-; done
}
