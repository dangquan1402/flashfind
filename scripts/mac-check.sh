#!/usr/bin/env bash
# One-shot macOS verification: tests, release build, atomic reinstall of
# ~/.local/bin/flashfind, MCP stdio handshake, FlashFind.app build + launch
# (snapshot, then quit). User-level only.
#   scripts/mac-check.sh [--no-app] [--snapshot PNG] [--query Q]
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
APP=1; SNAP=""; QUERY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --no-app) APP=0 ;;
    --snapshot) SNAP="$2"; shift ;;
    --query) QUERY="$2"; shift ;;
  esac
  shift
done
echo "== cargo test"
cargo test --workspace --release 2>&1 | grep -E "^test result|FAILED|panicked" || true
cargo test --workspace --release >/dev/null 2>&1 || { echo "TESTS FAILED"; exit 1; }
echo "== build + install"
cargo build --release -p flashfind-cli 2>&1 | tail -1
mkdir -p ~/.local/bin
cp target/release/flashfind ~/.local/bin/.flashfind.tmp
mv -f ~/.local/bin/.flashfind.tmp ~/.local/bin/flashfind
~/.local/bin/flashfind --version
echo "== MCP handshake"
OUT="$(mktemp)"
( printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"check","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_files","arguments":{"query":"readme","limit":3}}}'
  sleep 8 ) | ~/.local/bin/flashfind mcp > "$OUT" 2>/dev/null &
PID=$!
for _ in $(seq 1 60); do [ "$(wc -l < "$OUT")" -ge 3 ] && break; sleep 1; done
kill $PID 2>/dev/null || true
python3 - "$OUT" <<'PY'
import json,sys
lines=[json.loads(l) for l in open(sys.argv[1]) if l.strip()]
assert len(lines)>=3, f"only {len(lines)} replies"
print("server:", lines[0]["result"]["serverInfo"])
print("tools:", [t["name"] for t in lines[1]["result"]["tools"]])
print("find_files:", lines[2]["result"]["content"][0]["text"][:400])
PY
rm -f "$OUT"
if [ "$APP" = 1 ]; then
  echo "== app"
  scripts/build-app.sh > /tmp/flashfind-build-app.log 2>&1 || { grep -E "error" /tmp/flashfind-build-app.log | head -20; echo "APP BUILD FAILED"; exit 1; }
  tail -1 /tmp/flashfind-build-app.log
  ARGS=(--quit-after 1.5 --window-id-file /tmp/flashfind-wid.txt)
  [ -n "$SNAP" ] && ARGS+=(--snapshot "$SNAP")
  [ -n "$QUERY" ] && ARGS+=(--query "$QUERY")
  [ -n "${CWD:-}" ] && ARGS+=(--cwd "$CWD")
  rm -f /tmp/flashfind-wid.txt
  "$REPO/dist/FlashFind.app/Contents/MacOS/FlashFind" "${ARGS[@]}" &
  APID=$!
  for _ in $(seq 1 40); do kill -0 $APID 2>/dev/null || break; sleep 0.5; done
  kill $APID 2>/dev/null && echo "app did not quit by itself (killed)" || true
  echo "app status: $(tail -1 /tmp/flashfind-wid.txt 2>/dev/null || echo 'no window file')"
fi
echo "== ok"
