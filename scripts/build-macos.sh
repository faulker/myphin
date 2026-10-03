#!/usr/bin/env bash
# Build a release Myphin.app for macOS into dist/.
#
# `cargo build --release` gives a bare binary without the CSS and icons,
# because Dioxus only resolves `asset!()` paths when building through `dx`.
# This script runs `dx bundle`, which copies the assets into the .app.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "$(uname)" != "Darwin" ]]; then
  echo "error: this script builds a macOS .app and must run on macOS" >&2
  exit 1
fi

if ! command -v dx >/dev/null 2>&1; then
  echo "error: dioxus-cli not found. Install it with: cargo install dioxus-cli" >&2
  exit 1
fi

app="dist/Myphin.app"
rm -rf "$app"

dx bundle --desktop --release --package-types macos

if [[ ! -d "$app" ]]; then
  echo "error: dx bundle finished but $app is missing" >&2
  exit 1
fi

# Ad-hoc sign the whole bundle so the Info.plist and resources are sealed.
# Apple Silicon refuses to launch apps with a broken bundle signature.
codesign --force --deep --sign - "$app"

echo
echo "Built $app"
echo "Run it with: open $app"
echo "Install it with: cp -R $app /Applications/"
