#!/bin/sh
# flashfind installer (user-level, no sudo):
#   curl -fsSL https://raw.githubusercontent.com/dangquan1402/flashfind/main/install.sh | sh
# macOS arm64: downloads the release binary (sha256-checked).
# Elsewhere: builds the tagged release with cargo (Rust 1.88+).
# Installs to $FLASHFIND_BIN_DIR or ~/.local/bin, then prints MCP setup.
set -eu
REPO="dangquan1402/flashfind"
VERSION="${FLASHFIND_VERSION:-latest}"
BIN_DIR="${FLASHFIND_BIN_DIR:-$HOME/.local/bin}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$BIN_DIR"

say() { printf '%s\n' "$*"; }

if [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ]; then
  if [ "$VERSION" = latest ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
  else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
  fi
  ASSET="flashfind-macos-arm64.tar.gz"
  say "Downloading $BASE/$ASSET"
  curl -fsSL -o "$TMP/$ASSET" "$BASE/$ASSET"
  curl -fsSL -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256"
  (cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null) || { say "checksum mismatch"; exit 1; }
  tar -xzf "$TMP/$ASSET" -C "$TMP"
  SRC="$(find "$TMP" -type f -name flashfind -perm -u+x | head -1)"
  # Downloaded with curl, so no quarantine flag; clear it anyway if present.
  xattr -d com.apple.quarantine "$SRC" 2>/dev/null || true
else
  command -v cargo >/dev/null || { say "Needs Rust (https://rustup.rs) on this platform."; exit 1; }
  TAG_ARGS=""
  [ "$VERSION" != latest ] && TAG_ARGS="--tag $VERSION"
  say "Building flashfind with cargo (a minute or two)…"
  # shellcheck disable=SC2086
  cargo install --quiet --git "https://github.com/$REPO" $TAG_ARGS flashfind-cli --root "$TMP/cargo"
  SRC="$TMP/cargo/bin/flashfind"
fi

# Atomic replace: a running MCP server keeps its old inode.
cp "$SRC" "$BIN_DIR/.flashfind.tmp"
chmod 755 "$BIN_DIR/.flashfind.tmp"
mv -f "$BIN_DIR/.flashfind.tmp" "$BIN_DIR/flashfind"
BIN="$BIN_DIR/flashfind"
say "Installed $("$BIN" --version) to $BIN"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) say "(add $BIN_DIR to your PATH)";; esac

cat <<MSG

Try it:
  flashfind projects            # every git repo under ~ (first run indexes them, a few seconds)
  flashfind code router         # find a file across all of them
  flashfind code                # suggestions: what you were working on

Use it from your AI agent (MCP, stdio):

  Cursor: add to ~/.cursor/mcp.json
    {
      "mcpServers": {
        "flashfind": { "command": "$BIN", "args": ["mcp"] }
      }
    }

  Claude Code:
    claude mcp add --scope user flashfind -- $BIN mcp

  Codex CLI: add to ~/.codex/config.toml
    [mcp_servers.flashfind]
    command = "$BIN"
    args = ["mcp"]

Your picks and edits teach the ranking; they stay on this machine in
~/.local/share/flashfind (flashfind pick --show / --forget).
MSG
