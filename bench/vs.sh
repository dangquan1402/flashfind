#!/usr/bin/env bash
# flashfind vs fff vs fsearch, in the terminal.
#
#   bench/vs.sh <folder> [label] [-- extra flashfind-vs args]
#   bench/vs.sh linux        # shallow-clones torvalds/linux into $DATA and races on it
#   bench/vs.sh chromium     # same for chromium/chromium (~500k files, several GB)
#   bench/vs.sh repos        # synthetic 200-repo tree (node_modules, target, .venv ...)
#
# fff is fetched by cargo (pinned git rev of dmtrKovalenko/fff, its Rust core).
# fsearch (macOS only) is used from ../fsearch next to this repo; cloned if missing.
# Nothing is installed system-wide. Env: FLASHFIND_BENCH_DIR (default ~/dev/flashfind-bench).
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BENCH=${FLASHFIND_BENCH_DIR:-$HOME/dev/flashfind-bench}
DATA="$BENCH/datasets.noindex"   # .noindex: keep Spotlight out of it
mkdir -p "$DATA"
TARGET=${1:?usage: bench/vs.sh <folder|linux|chromium|repos> [label]}
shift
LABEL=""
if [ $# -gt 0 ] && [ "$1" != "--" ]; then LABEL=$1; shift; fi
[ "${1:-}" = "--" ] && shift

if [ "$(uname)" = Darwin ] && [ ! -d "$REPO/../fsearch" ]; then
  git clone -q https://github.com/noahdunnagan/fsearch "$REPO/../fsearch"
fi
if [ "$(uname)" != Darwin ] && [ ! -e "$REPO/../fsearch/Cargo.toml" ]; then
  # cargo still resolves the (macOS-only) path dependency
  git clone -q --depth 1 https://github.com/noahdunnagan/fsearch "$REPO/../fsearch"
fi
(cd "$REPO/bench/vs" && cargo build --release -q)
(cd "$REPO" && cargo build --release -q -p flashfind-cli)

case "$TARGET" in
  linux|chromium)
    URL=https://github.com/torvalds/linux.git; [ "$TARGET" = chromium ] && URL=https://github.com/chromium/chromium.git
    if [ ! -d "$DATA/$TARGET/.git" ]; then
      echo "shallow-cloning $URL into $DATA/$TARGET ..."
      git clone -q --depth 1 "$URL" "$DATA/$TARGET"
    fi
    ROOT="$DATA/$TARGET"; LABEL=${LABEL:-$( [ "$TARGET" = linux ] && echo "Linux kernel" || echo Chromium )} ;;
  repos)
    ROOT="$BENCH/repos-200-7.noindex"
    if [ ! -f "$ROOT.complete" ]; then
      rm -rf "$ROOT"
      "$REPO/target/release/flashfind" gen-repos --root "$ROOT" --repos 200 --seed 7 --queries "$BENCH/repos-queries-200-7.jsonl"
      touch "$ROOT.complete"
    fi
    LABEL=${LABEL:-"200 synthetic repos"} ;;
  *) ROOT=$TARGET; LABEL=${LABEL:-$(basename "$TARGET")} ;;
esac
exec "$REPO/target/release/flashfind" bench-vs --root "$ROOT" --label "$LABEL" "$@"
