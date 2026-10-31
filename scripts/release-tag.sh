#!/usr/bin/env bash
# Make the Release tag. Reads the Version from the project and puts the
# annotated git tag v<Version> on HEAD, so the two cannot disagree (the
# container shape's release-tag job checks the same thing again on the
# pushed Release tag). Annotated, because `git push --follow-tags` carries
# main and an annotated tag in one push and leaves a lightweight one behind.
#
# It refuses unless the tree is clean, HEAD is on main, HEAD is the bump
# commit (its Version differs from its parent's), and the Release tag is
# new. The Release tag belongs on the last commit of the batch, the bump:
# the Version the release build reads is then the one the tag names, and
# the container shape's gate reads the verdict of the head of the push.
#
# The Version is the kiewer package's, read from the checked-out tree;
# the parent commit's comes from [workspace.package] in its Cargo.toml as
# committed; the lock guard checks that the committed Cargo.lock carries
# the Version, since a --locked release build refuses one that does not.
#
# Usage: scripts/release-tag.sh
set -euo pipefail
cd "$(dirname "$0")/.."

refuse() {
  echo "release-tag: $1" >&2
  exit 1
}

[ -z "$(git status --porcelain)" ] \
  || refuse "the tree is not clean; commit or remove what git status shows"

branch="$(git symbolic-ref --quiet --short HEAD || true)"
[ "$branch" = main ] \
  || refuse "HEAD is not on main (it is on '${branch:-a detached commit}')"

version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "kiewer") | .version')"
[ -n "$version" ] || refuse "no Version could be read"

# The two shapes a Release takes: a Release and a Release candidate.
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]] \
  || refuse "the Version '$version' is neither X.Y.Z nor X.Y.Z-rc.N, the two shapes a Release takes"

parent="$(git show 'HEAD^:Cargo.toml' 2>/dev/null | sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' || true)"
# A parent whose Version cannot be read must not pass for a different one.
if git rev-parse --quiet --verify 'HEAD^' >/dev/null && [ -z "$parent" ]; then
  refuse "cannot read the parent commit's Version, so cannot tell whether HEAD is the bump commit"
fi
[ "$parent" != "$version" ] \
  || refuse "HEAD is not the bump commit: its parent already says $version. The Release tag goes on the bump, the last commit before the push"

# The lock file as committed: a --locked release build reads it.
lock_version() {
  awk '/^\[\[package\]\]/ { in_main = 0 }
       /^name = "kiewer"$/ { in_main = 1 }
       in_main && /^version = / { sub(/^version = "/, ""); sub(/"$/, ""); print; exit }'
}
locked="$(git show 'HEAD:Cargo.lock' 2>/dev/null | lock_version || true)"
[ -n "$locked" ] \
  || refuse "Cargo.lock as committed names no Version for kiewer; the lock file must be committed with the bump"
[ "$locked" = "$version" ] \
  || refuse "Cargo.lock as committed says $locked for kiewer where the manifest says $version; run 'cargo update --workspace' and commit the lock file with the bump"

tag="v$version"
git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null \
  && refuse "the Release tag $tag already exists; a new Release is a new Version"

git tag --annotate "$tag" --message "kiewer $version"
echo "release-tag: $tag is on $(git rev-parse --short HEAD); push with: git push --follow-tags"
