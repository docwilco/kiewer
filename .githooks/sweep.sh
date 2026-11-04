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
# not be compiled again.

sweep_incremental() {  # $1: the hook's name, $2: a target directory
  local old
  old=$(find "$2"/*/incremental -mindepth 1 -maxdepth 1 -type d -mtime +3 \
    2>/dev/null || true)
  [ -n "$old" ] || return 0
  echo "$1: removing $(printf '%s\n' "$old" | wc -l) incremental" \
    "directories not compiled in three days under $2"
  printf '%s\n' "$old" | xargs -d '\n' rm -rf --
}

# The checkout's own target, by age. Skipped while a build holds a profile's
# lock, and when cargo-sweep is missing, with a line saying so either way.
sweep_checkout() {  # $1: the hook's name, $2: the checkout's root
  local lock busy=0
  for lock in "$2"/target/*/.cargo-lock; do
    if [ -e "$lock" ] && ! flock -n "$lock" true; then busy=1; fi
  done
  if [ "$busy" = 1 ]; then
    echo "$1: a build holds the checkout's target; its sweep is skipped."
    return 0
  fi
  sweep_incremental "$1" "$2/target"
  if command -v cargo-sweep >/dev/null; then
    cargo sweep --time 3 "$2"
  else
    echo "$1: cargo-sweep is missing (cargo install cargo-sweep --locked); no sweep."
  fi
}
