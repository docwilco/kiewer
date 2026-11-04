#!/usr/bin/env bash
# Whether a run of the CI workflow's release job cuts the Version, as the
# job's outputs: version, kind (release or candidate), cut and prune.
#
# A run cuts its Version only when it sorts above every Release in git, by
# git's version order with a candidate before its Release (sort -V puts
# 0.2.0 before 0.2.0-rc.1). A Version already tagged, or below the newest
# Release, is a plain push: two Bumps whose runs finish out of order, or a
# late rerun of a red older Bump, would otherwise move production back to
# older code on a green run (#97).
#
# The Releases are git's tags of the two shapes, read from the remote: CI's
# checkout is shallow and carries none. A tag of neither shape (vnext, v1)
# is no Release and takes no place, as in scripts/prune-images.sh, so a
# stray one cannot stop every later cut. The read happens in an assignment,
# so a failed ls-remote stops the script instead of reading as no tags,
# which would cut a Version that may be tagged already.
#
# The prune repeats on any rerun. A failed prune comes after the tag, so
# the run is red with the Version tagged, and a rerun finds it so; without
# this the registry stays over count until the next Release. A first
# attempt prunes only when it cuts.
#
# Usage: scripts/release-cut.sh <Version>
#
# Needs a `git` remote named origin, and the runner's GITHUB_OUTPUT;
# GITHUB_RUN_ATTEMPT is 1 when unset.
set -euo pipefail
# A failure inside a command substitution stops the script too.
shopt -s inherit_errexit
cd "$(dirname "$0")/.."

version="${1:?usage: $0 <Version>}"
output="${GITHUB_OUTPUT:?set GITHUB_OUTPUT}"
attempt="${GITHUB_RUN_ATTEMPT:-1}"

# The two shapes a Release takes, here and in scripts/prune-images.sh.
release_shape='^[0-9]+\.[0-9]+\.[0-9]+$'
candidate_shape='^[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$'

if [[ "$version" =~ $release_shape ]]; then
  kind=release
elif [[ "$version" =~ $candidate_shape ]]; then
  kind=candidate
else
  echo "::error::the Version '$version' is neither X.Y.Z nor X.Y.Z-rc.N, the two shapes a Release takes"
  exit 1
fi

# The newest of the Versions given, by git's version order. Git sorts refs
# only, so the Versions become tags in a throwaway repository, each on the
# empty tree. The body is a subshell, so its EXIT trap removes the
# repository on a failure too.
newest_by_git() (
  repo="$(mktemp -d)"
  trap 'rm -rf "$repo"' EXIT
  git init --quiet --bare "$repo"
  tree="$(git --git-dir="$repo" mktree </dev/null)"
  for v in "$@"; do echo "create refs/tags/v$v $tree"; done |
    git --git-dir="$repo" update-ref --stdin
  git --git-dir="$repo" -c versionsort.suffix=-rc tag --list --sort=version:refname |
    tail -n 1 | sed 's/^v//'
)

versions="$(git ls-remote --refs --tags origin 'v*' | sed -n 's|^.*refs/tags/v||p')"
mapfile -t releases < <(grep -E "$release_shape|$candidate_shape" <<<"$versions" || true)

tagged=false
for release in "${releases[@]}"; do
  [ "$release" != "$version" ] || tagged=true
done

if [ "$tagged" = true ]; then
  echo "v$version is in git: a plain push, nothing to cut"
  cut=false
else
  above="$(newest_by_git "$version" "${releases[@]}")"
  if [ "$above" = "$version" ]; then
    echo "v$version sorts above every Release in git: this run cuts it"
    cut=true
  else
    echo "v$version sorts below v$above, already in git: a plain push, nothing to cut"
    cut=false
  fi
fi

prune="$cut"
[ "$attempt" = 1 ] || prune=true
{
  echo "version=$version"
  echo "kind=$kind"
  echo "cut=$cut"
  echo "prune=$prune"
} >> "$output"
