#!/usr/bin/env bash
# Point the Homebrew tap (dangquan1402/homebrew-flashfind) at release TAG:
# recompute the source tarball sha256 and push Formula/flashfind.rb.
# Creates the (public) tap repo the first time. Needs gh.
#   scripts/bump-formula.sh v0.2.0
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
TAG="${1:?usage: scripts/bump-formula.sh vX.Y.Z}"
TAP="dangquan1402/homebrew-flashfind"
SRC="https://github.com/dangquan1402/flashfind/archive/refs/tags/$TAG.tar.gz"
SHA="$(curl -fsSL "$SRC" | shasum -a 256 | cut -d' ' -f1)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
if ! gh repo view "$TAP" >/dev/null 2>&1; then
  gh repo create "$TAP" --public --description "Homebrew tap for flashfind: find the file you meant across all your git projects" >/dev/null
fi
git clone -q "https://github.com/$TAP.git" "$WORK/tap" 2>/dev/null || { mkdir -p "$WORK/tap"; git -C "$WORK/tap" init -q; git -C "$WORK/tap" remote add origin "https://github.com/$TAP.git"; }
mkdir -p "$WORK/tap/Formula"
sed -e "s|@URL@|$SRC|" -e "s|@SHA256@|$SHA|" "$REPO/packaging/homebrew/flashfind.rb.in" > "$WORK/tap/Formula/flashfind.rb"
[ -f "$WORK/tap/README.md" ] || cat > "$WORK/tap/README.md" <<'MD'
# homebrew-flashfind

```sh
brew install dangquan1402/flashfind/flashfind
```

Builds [flashfind](https://github.com/dangquan1402/flashfind) from its tagged
release. The formula is bumped by `scripts/bump-formula.sh` in the main repo
(run by `scripts/release.sh`, or by the release workflow when the
`HOMEBREW_TAP_TOKEN` secret is set).
MD
cd "$WORK/tap"
git add -A
if git diff --cached --quiet; then echo "formula already at $TAG"; exit 0; fi
git -c user.name="${GIT_AUTHOR_NAME:-flashfind release}" -c user.email="${GIT_AUTHOR_EMAIL:-noreply@github.com}" commit -qm "flashfind ${TAG#v}"
git branch -M main
git push -q -u origin main
echo "tap bumped to $TAG ($SHA)"
