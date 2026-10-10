#!/usr/bin/env bash
# Benchmark flashfind vs upstream fsearch on a synthetic tree.
#   bench/run.sh [FILES=1000000] [SEED=42]
# Env: FLASHFIND_BENCH_DIR (default ~/dev/flashfind-bench), FSEARCH_DIR
# (default ../fsearch next to this repo), REPEAT (default 5).
set -euo pipefail
FILES=${1:-1000000}
SEED=${2:-42}
REPEAT=${REPEAT:-5}
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BENCH=${FLASHFIND_BENCH_DIR:-$HOME/dev/flashfind-bench}
FSEARCH=${FSEARCH_DIR:-$REPO/../fsearch}
OUT="$REPO/bench/out"
TREE="$BENCH/tree-$FILES-$SEED"
Q="$BENCH/queries-$FILES-$SEED.jsonl"
mkdir -p "$OUT" "$BENCH"

echo "== build"
(cd "$REPO" && cargo build --release -q -p flashfind-cli)
(cd "$REPO/bench/fsearch-baseline" && cargo build --release -q)
FF="$REPO/target/release/flashfind"
FS="$REPO/bench/fsearch-baseline/target/release/fsearch-baseline"

if [ ! -d "$TREE" ]; then
  echo "== generate $FILES files (seed $SEED) in $TREE"
  "$FF" gen --root "$TREE" --files "$FILES" --seed "$SEED" --queries "$Q"
fi

echo "== warm-up crawl (fills the metadata cache; not measured)"
"$FF" index "$TREE" -o "$BENCH/ff.ffx" >/dev/null 2>&1

echo "== index builds"
"$FF" index "$TREE" -o "$BENCH/ff.ffx" --stats-json "$OUT/ff-build.json"
"$FS" build --root "$TREE" --index "$BENCH/fs.idx" --stats "$OUT/fs-build.json"
"$FF" index "$TREE" -o "$BENCH/ff-portable.ffx" --portable --stats-json "$OUT/ff-build-portable.json"
rm -f "$BENCH/ff-portable.ffx"

echo "== queries (repeat $REPEAT)"
"$FF" bench -i "$BENCH/ff.ffx" --queries "$Q" --repeat "$REPEAT" --out "$OUT/ff-query.json"
"$FS" query --index "$BENCH/fs.idx" --queries "$Q" --repeat "$REPEAT" --out "$OUT/fs-query.json"

{
  if command -v sw_vers >/dev/null; then
    echo "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)), $(uname -m)"
    echo "CPU: $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores ($(sysctl -n hw.perflevel0.physicalcpu 2>/dev/null || echo '?') performance)"
    echo "RAM: $(( $(sysctl -n hw.memsize) / 1073741824 )) GB"
    echo "Disk: $(diskutil info / 2>/dev/null | awk -F: '/File System Personality/ {gsub(/^ +/, "", $2); print $2}')"
  else
    echo "$(uname -srm)"; echo "CPU: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-) x$(nproc)"
  fi
  echo "$(rustc --version)"
  echo "flashfind $(git -C "$REPO" rev-parse --short HEAD), fsearch $(git -C "$FSEARCH" rev-parse --short HEAD 2>/dev/null || echo '?') (upstream main)"
  echo "tree: $FILES files + $((FILES / 10)) dirs, seed $SEED; queries: $(wc -l < "$Q" | tr -d ' ') lines; $REPEAT passes"
  echo "flashfind portable read_dir crawl (for comparison): $(python3 -c "import json;print(round(json.load(open('$OUT/ff-build-portable.json'))['crawl_ms']), 'ms')" 2>/dev/null || echo n/a)"
} > "$OUT/machine.txt"

NOTES="$REPO/bench/NOTES.md"
"$FF" report --machine "$OUT/machine.txt" --ff-build "$OUT/ff-build.json" --ff-query "$OUT/ff-query.json" \
  --fs-build "$OUT/fs-build.json" --fs-query "$OUT/fs-query.json" $( [ -f "$NOTES" ] && echo --notes "$NOTES" ) \
  --out "$REPO/bench/RESULTS.md"
cat "$REPO/bench/RESULTS.md"
