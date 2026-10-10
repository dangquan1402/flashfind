# flashfind

A whole-disk file **name** search engine for macOS (portable to Linux), written
in Rust and built to be linked **in-process** into an app and queried on every
keystroke. It is a re-engineering of the ideas in
[fsearch](https://github.com/noahdunnagan/fsearch), aimed at lower and
more predictable per-keystroke latency, typo tolerance anywhere in a word, and
an FFI surface a Swift app can call directly.

> **Status: phase 0 + phase 1 MVP.** The index, crawler, query language,
> search engine, CLI, C ABI and benchmark harness work and are tested on Linux
> and macOS (Apple Silicon). There is **no live updating yet** (FSEvents is a
> TODO; re-run `flashfind index`), no content search and no app. Not published
> to crates.io.

## Credits

flashfind stands on two MIT-licensed projects. Full license texts are in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

- **[fsearch](https://github.com/noahdunnagan/fsearch) by Noah Dunnagan**: the
  main inspiration and the baseline it is benchmarked against.
  - *Code adapted:* the macOS `getattrlistbulk(2)` crawler
    (`crates/flashfind-core/src/walk/macos.rs`: attribute buffer parsing,
    `openat` fan-out over a rayon scope, the `setiopolicy_np` "don't download
    iCloud files" policy, raising the fd limit) and the file-type extension
    table behind `type:image|video|…`.
  - *Ideas:* interned distinct names with a character-class bitmask
    prefilter; a name → entries inverted list, so each distinct name is scored
    once; entries stored in depth-first order, so a folder is a contiguous
    range (`in:` scoping); location priors; narrowing the previous query's
    survivors while you type; the filter syntax (`ext:`, `type:`, `size:`,
    `mtime:`, `in:`).
  - `bench/fsearch-baseline` links upstream fsearch as a library to measure it.
- **[fff](https://github.com/dmtrKovalenko/fff) by Dmitriy Kovalenko**: no code
  copied. Ideas taken from it: the engine as an embeddable library with a C ABI
  (like `fff-c`) instead of a daemon behind a socket; typo tolerance that grows
  with query length (fff's matcher, frizbee); and a bigram prefilter (fff uses
  one for content search; flashfind uses a per-name bigram signature to skip
  names that can't be within the typo budget). frizbee itself is not used.

## What is different from fsearch

| | fsearch | flashfind |
|---|---|---|
| Process model | daemon + Unix socket; FinderSearch adds a `stdio` child process and a 150 ms debounce | library linked into the app (`flashfind-ffi`), one call per keystroke, cancellable from any thread |
| Typos | 1 edit for words of 5+ letters; first letter must match | 1 edit at 5–8 bytes, 2 at 9+, **anywhere** incl. the first letter; adjacent swaps count as one edit (bit-parallel Damerau/OSA, Myers–Hyyrö) |
| 1–3 letter queries | score every name | answered from a sorted-name prefix range, stopping early once no remaining name can enter the page; result proven identical to a full scan, else it falls back |
| Typing | narrows from the last query | keeps 8 recent survivor sets (so backspace is cheap too) |
| Typo work | always | skipped when clean matches already fill the page above any possible typo score |
| `mtime:3d` | 1-second window (bug) | "within 3 days" |
| `mtime:>140y` | u32 wrap-around (bug) | saturating i64 arithmetic |
| Paths deeper than 256 levels | truncated (bug) | no depth limit |
| Sizes / mtimes | u32 (sizes lossy above 2 GiB) | exact u64 / i64 |
| Results | full list | top-k first page, with exact or lower-bound total |

## Benchmarks (Apple M5 Pro, 1.1 M-entry synthetic tree)

Both engines run in-process, on the same tree and queries, through their
library APIs. Full table and method: [bench/RESULTS.md](bench/RESULTS.md).

| | flashfind | fsearch |
|---|---|---|
| all queries p50 / p95 / p99 | **472 µs / 2.81 ms / 6.94 ms** | 1.11 ms / 8.71 ms / 22.0 ms |
| per keystroke (typing a name) p50 / p95 | **156 µs / 2.64 ms** | 1.16 ms / 8.24 ms |
| 1-letter queries p50 | **1.06 ms** | 5.24 ms |
| typo in first letter, hit@1 | **100%** | 0% (by design) |
| typo_drop hit@1 | 90% | **97%** |
| multi-word (folder + name) hit@1 | 95% | **100%** |
| crawl (getattrlistbulk) | 15.6 s | **14.9 s** |
| index build | 550 ms | **217 ms** |
| index file | 106 MB | **56 MB** |
| RSS while querying | 133 MB | **94 MB** |

Read these numbers with care:

- **Where flashfind is worse:** a larger index (it keeps lower- and
  original-case names, a 16-byte bigram signature per name, and exact
  64-bit sizes/mtimes) and more RAM; a 2.5× slower index build; a slightly
  slower crawl; and lower hit@1 on dropped-letter typos and folder+name queries.
  The multi-word case is because its ancestor-folder scoring is a flat bonus.
- **Noisy machine:** load average was 60–170 during the run (Spotlight and
  other user processes), so each engine ran twice, alternating, and the better
  run is shown. Expect the absolute numbers to move between runs; the ratios
  were stable across the runs we made.
- **Synthetic data:** names are far more unique than on a real disk (911 k
  distinct names per 1.1 M entries; fsearch reports ~2 M per 7.5 M). The
  generator and the hit@1 targets are flashfind's own. This is an engine
  comparison, not a whole-disk prediction. Whole-disk runs are not done yet.
- fsearch's socket/CLI round trip is *not* included; in a real app it adds to
  fsearch's side.

## Build and use

Requires Rust 1.88+.

```sh
cargo build --release
cargo test --release --workspace

# index a folder (or / for the whole disk, with Full Disk Access)
./target/release/flashfind index ~/Documents -o ~/.flashfind/index.ffx
./target/release/flashfind search 'invoce ext:pdf mtime:<30d'
./target/release/flashfind search --explain --limit 20 'q3 reprot'
./target/release/flashfind help query
```

Query language: plain words are fuzzy and must all match the name or a parent
folder; `'x` substring, `^x` prefix, `x$` suffix, `=x` exact, `!x`/`-x`
exclude; filters `ext:` `type:` `size:` `mtime:` `in:` `limit:`.

### Embedding (C / Swift)

`crates/flashfind-ffi` builds `libflashfind.a` / `.dylib`; the header is
[`crates/flashfind-ffi/include/flashfind.h`](crates/flashfind-ffi/include/flashfind.h).
Open an index (mmap, about 8 ms for 1.1 M entries), create one session per
search field, and for each keystroke cancel the previous token and call
`ff_search` on a worker thread. See `crates/flashfind-ffi/examples/smoke.c`.

### Reproducing the benchmark

```sh
git clone https://github.com/noahdunnagan/fsearch ../fsearch   # next to this repo
bench/run.sh 1000000 42   # writes ~1.1 M files under ~/dev/flashfind-bench, then bench/RESULTS.md
```

## Layout

- `crates/flashfind-core`: crawler (`walk/`), index format (`index.rs`,
  mmap-able `FFIDX004`), matcher (`matcher.rs`), query parser (`query.rs`) and
  engine (`search.rs`: name phase → entry phase, narrowing, prefix early exit,
  clean-first typo gate, cancellation)
- `crates/flashfind-cli`: the `flashfind` binary (`index`, `search`, `gen`,
  `bench`, `report`)
- `crates/flashfind-ffi`: C ABI
- `bench/`: run script, fsearch baseline harness, results

## Roadmap

FSEvents live updates (overlay + tombstones), searching while the first crawl
runs, better ancestor-folder scoring, a smaller index (drop duplicate-case name
copies, narrower size/mtime), whole-disk and real-tree benchmarks
(Chromium, Linux), a Swift demo app, then content search.

## License

MIT, see [LICENSE](LICENSE). Third-party notices are in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
