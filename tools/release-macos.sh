#!/usr/bin/env bash
# Builds dist/IDM2-<version>-macos-universal.zip on a Mac (or the macOS CI runner):
#   IDM2.app (one binary for Apple Silicon + Intel), extension/ (Chrome/Edge), extension-firefox/.
# Afterwards, older macOS builds are deleted from dist/ (only the latest is kept).
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
stage="dist/IDM2-$version-macos"
zip="dist/IDM2-$version-macos-universal.zip"

rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

rm -rf "$stage" "$zip"
app="$stage/IDM2.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$stage/extension" "$stage/extension-firefox"
lipo -create -output "$app/Contents/MacOS/idm2" \
    target/aarch64-apple-darwin/release/idm2 target/x86_64-apple-darwin/release/idm2
cp macos/idm2.icns "$app/Contents/Resources/"
sed "s/@VERSION@/$version/g" macos/Info.plist > "$app/Contents/Info.plist"
# Apple Silicon only runs signed code; an ad-hoc signature is enough for that. It is not a
# Developer ID signature, so Gatekeeper still asks users to confirm the first launch.
codesign --force --deep --sign - "$app"

for dir in extension extension-firefox; do
    cp -R extension/background.js extension/icons "$stage/$dir/"
done
cp extension/manifest.json "$stage/extension/manifest.json"
cp extension/manifest.firefox.json "$stage/extension-firefox/manifest.json"

ditto -c -k --sequesterRsrc "$stage" "$zip"
echo "Packaged $zip ($(du -h "$zip" | cut -f1))"

for old in dist/*-macos*; do
    case "$old" in
        "$stage" | "$zip") ;;
        *) rm -rf "$old"; echo "Removed old build $old" ;;
    esac
done
