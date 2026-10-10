#!/usr/bin/env bash
# Cut a release from this Mac (arm64): build, tag, GitHub release with the
# binary tarball (+ the app zip), and bump the Homebrew tap formula.
#   scripts/release.sh v0.2.0 [--notes FILE]
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
TAG="${1:?usage: scripts/release.sh vX.Y.Z}"; shift
NOTES=""
[ "${1:-}" = "--notes" ] && NOTES="$2"
VER="${TAG#v}"
grep -q "^version = \"$VER\"" Cargo.toml || { echo "Cargo.toml version is not $VER"; exit 1; }
[ "$(uname -sm)" = "Darwin arm64" ] || { echo "run on an Apple Silicon Mac"; exit 1; }
[ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "working tree not clean"; exit 1; }

OUT="dist/release"
rm -rf "$OUT"; mkdir -p "$OUT/flashfind-$TAG-macos-arm64"
cargo build --release -p flashfind-cli
cp target/release/flashfind LICENSE THIRD_PARTY_NOTICES.md README.md "$OUT/flashfind-$TAG-macos-arm64/"
strip -S "$OUT/flashfind-$TAG-macos-arm64/flashfind" 2>/dev/null || true
tar -C "$OUT" -czf "$OUT/flashfind-$TAG-macos-arm64.tar.gz" "flashfind-$TAG-macos-arm64"
cp "$OUT/flashfind-$TAG-macos-arm64.tar.gz" "$OUT/flashfind-macos-arm64.tar.gz"
(cd "$OUT" && for f in flashfind-$TAG-macos-arm64.tar.gz flashfind-macos-arm64.tar.gz; do shasum -a 256 "$f" > "$f.sha256"; done)
scripts/build-app.sh >/dev/null
ditto -c -k --keepParent dist/FlashFind.app "$OUT/FlashFind-$TAG-macos-arm64.zip"

git tag -a "$TAG" -m "flashfind $TAG" 2>/dev/null || echo "tag $TAG exists"
git push -q origin "$TAG"
ARGS=(--title "flashfind $TAG")
if [ -n "$NOTES" ]; then ARGS+=(--notes-file "$NOTES"); else ARGS+=(--generate-notes); fi
gh release view "$TAG" >/dev/null 2>&1 || gh release create "$TAG" "${ARGS[@]}"
gh release upload "$TAG" --clobber "$OUT"/flashfind-*.tar.gz "$OUT"/flashfind-*.sha256 "$OUT"/FlashFind-*.zip

scripts/bump-formula.sh "$TAG"
echo "released $TAG"
