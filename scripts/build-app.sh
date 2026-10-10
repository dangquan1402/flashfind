#!/usr/bin/env bash
# Build dist/FlashFind.app: the Rust core as a static library linked
# in-process into a SwiftUI/AppKit app, ad-hoc signed. macOS 15+, Xcode or
# the Command Line Tools (Swift 6), and Rust.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
cargo build --release -p flashfind-ffi
mkdir -p dist/lib
# Only the static archive goes in the link folder, so the app has no dylib dependency.
cp target/release/libflashfind.a dist/lib/libflashfind.a
cd apps/macos
swift build -c release --product FlashFind -Xlinker -L"$REPO/dist/lib"
BIN="$(swift build -c release --show-bin-path)/FlashFind"
APP="$REPO/dist/FlashFind.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/FlashFind"
cp Resources/Info.plist "$APP/Contents/Info.plist"
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --verbose=1 "$APP"
echo "built $APP ($(du -sh "$APP" | cut -f1))"
