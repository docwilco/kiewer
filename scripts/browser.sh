#!/usr/bin/env bash
# The browser the browser stage of scripts/checks.sh drives: Chrome for
# Testing's chrome-headless-shell, pinned here by version and by the
# SHA-256 of its zip. This is the one place the pin lives, as
# scripts/wasm-bindgen-version.sh is for wasm-bindgen: CI and a dev box
# (README, Development) install with this script, and the stage finds the
# executable through it, so a bump here moves both. Linux x86-64 only.
#
# The Dockerfile installs none of this: the production image is
# runtime-only and runs no tests.
#
# Usage: scripts/browser.sh path       the pinned version's executable
#        scripts/browser.sh install    download, verify and unpack it there
#        scripts/browser.sh packages   the Ubuntu packages it links against
set -euo pipefail

version=156.0.8078.80
sha256=547dcd374e02961bf998f6046a267644f8502225c36bb78ca307dab32a0b3eb4
# The shared libraries the executable needs that a bare Ubuntu 24.04 lacks
# (`ldd` lists them as not found), by their 24.04 package names; CI's test
# job installs these on ubuntu-latest, so a runner image whose names differ
# fails its apt step until this list follows.
packages=(
  libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libatspi2.0-0t64
  libgbm1 libnspr4 libnss3 libxcomposite1 libxdamage1 libxfixes3
  libxkbcommon0 libxrandr2
)

dir="${XDG_CACHE_HOME:-$HOME/.cache}/kiewer/chrome-headless-shell/$version"
executable="$dir/chrome-headless-shell-linux64/chrome-headless-shell"

case "${1:-}" in
  path) echo "$executable" ;;
  packages) echo "${packages[*]}" ;;
  install)
    if [ -x "$executable" ]; then
      echo "chrome-headless-shell $version is installed at $executable"
      exit 0
    fi
    [ "$(uname -sm)" = "Linux x86_64" ] \
      || { echo "chrome-headless-shell is pinned for Linux x86-64 only" >&2; exit 1; }
    # Unpacked beside the final directory and moved into place whole, so
    # an interrupted install leaves no executable for the stage to find.
    mkdir -p "$(dirname "$dir")"
    work="$(mktemp -d "$dir.XXXXXX")"
    trap 'rm -rf "$work"' EXIT
    curl -fsSL -o "$work/browser.zip" \
      "https://storage.googleapis.com/chrome-for-testing-public/$version/linux64/chrome-headless-shell-linux64.zip"
    echo "$sha256  $work/browser.zip" | sha256sum -c --quiet -
    unzip -q "$work/browser.zip" -d "$work/unpacked"
    # -T: when another install got there first, fail rather than nest
    # this copy inside the directory the stage looks in.
    mv -T "$work/unpacked" "$dir"
    echo "chrome-headless-shell $version installed at $executable"
    ;;
  *) echo "usage: $0 path|install|packages" >&2; exit 2 ;;
esac
