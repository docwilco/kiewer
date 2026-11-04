#!/usr/bin/env bash
# Prune the registry's Release tags, after a cut (the CI workflow's release
# job). The newest 3 Releases and the newest 1 candidate keep their tags;
# every other tag of either shape is deleted. The image keeps its
# :<short hash> and falls to the registry's cleanup policy, which holds the
# 5 most recent images (infra/registry.tf has why two things hold the
# counts). `latest` and the hash tags are of neither shape, so they stay.
#
# The list of Releases is git's tags, read from the remote: CI's checkout
# is shallow and carries none. The registry says only which tags there are
# to delete, so a Release-shaped tag git never made (a cut whose deploy
# failed) goes too. A second run finds nothing to delete.
#
# Usage: scripts/prune-images.sh
#
# Needs a `git` remote named origin, gcloud signed in with repoAdmin on the
# repository, and GCP_PROJECT_ID in the environment (the same value as the
# repository variable CI reads).
set -euo pipefail

project="${GCP_PROJECT_ID:?set GCP_PROJECT_ID}"
image="europe-west4-docker.pkg.dev/$project/kiewer/kiewer"

# The two shapes a Release takes.
release='^[0-9]+\.[0-9]+\.[0-9]+$'
candidate='^[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$'

newest() {  # $1: a shape, $2: a count; the Versions on stdin
  { grep -E "$1" || true; } | sort -V | tail -n "$2"
}

versions="$(git ls-remote --refs --tags origin 'v*' | sed -n 's|^.*refs/tags/v||p')"
keep="$(newest "$release" 3 <<<"$versions"; newest "$candidate" 1 <<<"$versions")"

# A prune follows a cut, so git lists at least the Release just made. An
# empty list is a read that went wrong, and taken as it stands it would
# delete every Release in the registry.
if [ -z "$keep" ]; then
  echo "prune-images: git lists no Release on origin; deleting nothing" >&2
  exit 1
fi
echo "prune-images: keeping $(tr '\n' ' ' <<<"$keep")"

tags="$(gcloud artifacts docker tags list "$image" --format='value(tag.basename())')"
stale="$(grep -E "$release|$candidate" <<<"$tags" | grep -Fxv -f <(echo "$keep") || true)"

for tag in $stale; do
  echo "prune-images: deleting $image:$tag"
  gcloud artifacts docker tags delete "$image:$tag" --quiet
done
