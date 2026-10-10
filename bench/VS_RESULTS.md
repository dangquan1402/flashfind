# flashfind vs fff vs fsearch (terminal race)

Produced by `flashfind bench-vs` (harness: `bench/vs`, driver: `bench/vs.sh`).
Method mirrors fsearch's own `demo/vs_fff.py`: 300 file names that are unique
among the files `fd` would list (not hidden, not gitignored), each queried
as-is and with four one-letter typos (swap / drop / extra / substitute, never
the first letter). Added on top: folder+name queries, per-keystroke latency
(every prefix of 40 names), and on the multi-repo tree `repo:` queries and
junk-in-top-10. All three engines run in one process, warm, top 50, and take
turns going first. Cold build and memory are measured per engine in separate
child processes.

- **Datasets.** Chromium and Linux are *tree replicas* (`bench/vs/replica.py`):
  the exact file paths of HEAD of `chromium/chromium` (GitHub mirror,
  submodules not expanded) and `torvalds/linux`, with their real `.gitignore`
  files, empty contents, committed to a local git repo. The machine's network
  was too slow for full checkouts. Name search sees the same names, depths
  and ignore rules; contents differ, so this is valid for name search only.
- **flashfind** = the code-projects engine (`flashfind-code`: gitignore-aware,
  junk excluded). **fff** = `fff-search` 0.11.0 Rust core (git rev
  `65119f9`), `FilePicker::fuzzy_search` with its Python binding's defaults;
  content indexing, mmap warmup and the watcher are off (name search only).
  **fsearch** = upstream library (`walk::scan` + `Index::build` + `Searcher`)
  on the same folder.
- **Machine load.** These runs had load averages of 27–35 from unrelated
  work (54 logged-in sessions, Xcode, simulators). That is lower than the
  first run (27–114) but not quiet. Ratios between engines in the same run are
  more meaningful than absolute numbers. Re-run on a quiet machine before
  quoting.
- **Entry counts differ.** flashfind and fsearch count files plus folders. fff
  counts files only. fsearch also indexes ignored and junk files.

## Before / after the optimization pass (Chromium tree replica, medians)

| metric | flashfind before | flashfind after | fff | fsearch |
|---|---|---|---|---|
| exact name | 3.3 ms | 0.63 ms | 22 ms | **0.57 ms** |
| with a typo | 2.9 ms | 0.62 ms | 22 ms | **0.52 ms** |
| folder + name | 11 ms | 3.3 ms | 5.7 ms | **1.7 ms** |
| per keystroke | 0.25 ms | **0.22 ms** | 17 ms | 0.39 ms |
| ready, cold | 4.9 s | 1.4 s | 2.2 s | **0.93 s** |
| ready, warm | 33 ms | 3.6 ms | n/a | **2.1 ms** |
| memory | 204 MB | **74 MB** | 207 MB | 132 MB |
| right file first (exact / typo) | 99% / 95% | 99% / 95% | 100% / 86% | 100% / 98% |

"Before" is the earlier run (load 27–114), "after" this one (load 27–35), so
part of every engine's speedup is the quieter machine. Compare engines within
a row of the same run: fsearch went from 3× faster to 1.1–1.2× faster.
flashfind's results did not change: on all 2,683 benchmark queries the top 50
(paths, scores, totals) are identical before and after, and the index is
byte-identical (except its build timestamp) on Chromium, Linux, the 200-repo
tree and two real checkouts.

What changed (profiled first: the whole one-shot cost was the scan over
418k unique names; repo masks, the 150-hit fetch, hit conversion and JSON
were all under 1%):

1. **Clean match only when every token character class is present.** Before,
   all ~81k names passing the loose typo mask ran the subsequence scan. Now
   only the ~7k that have every class do.
2. **Exact typo prefilters before the OSA DP:** the q-gram count lemma (each
   edit destroys at most q+1 of the token's q-grams; tested over the name's
   bytes with a direct-indexed trigram set), plus a check that one of the 2k+1
   token pieces occurs verbatim. OSA runs dropped from ~30k to ~2.7k names
   per query. A randomized property test checks that these filters never
   reject a name the OSA path accepts.
3. **Bigram-signature test folded into the branchless gather loop**, and
   `may_match` made popcount-free for k ≤ 2.
4. **4k-name scan chunks** instead of 16k, for better load balance across
   cores (largest single win after 1 and 2).
5. **Cold build:** listings built from raw path bytes (985 → 234 ms). Then the
   crawl writes straight into per-folder listings: the `ignore` crate still
   decides inclusion, its `filter_entry` runs for a folder's accepted children
   on the thread that read the folder, and macOS reads sizes and mtimes with
   one getattrlistbulk call per folder instead of an lstat per entry. There is
   no path per entry and no global path sort.
6. **Memory:** most of the old 204 MB was transient crawl data (a `Vec<u8>`
   path per entry) that macOS malloc kept as footprint. The per-folder crawl
   peaks at about 80 MB. `release_memory()` (malloc_zone_pressure_relief /
   malloc_trim) runs after builds and compactions.

Tried and dropped: an early exit inside the q-gram count plus lazy
original-case fetching (identical results, no measurable gain), and 2k chunks
(slower than 4k).

### Chromium (tree replica)

507,278 visible files (not hidden, not gitignored; 555,555 entries in total) · Apple M5 Pro (15 cores, 52 GB) · macOS 26.5.1 · load 32.1 before, 35.2 after · 1,789 name queries, 894 keystrokes · top 50 · warm

| class | engine | n | p50 | p95 | p99 | right file first | junk in top 10 |
|---|---|---|---|---|---|---|---|
| exact | flashfind | 300 | 0.63 ms | 3.3 ms | 8.8 ms | 99% | 0.0% |
| exact | fff | 300 | 22 ms | 38 ms | 54 ms | 100% | 0.6% |
| exact | fsearch | 300 | **0.57 ms** | 2.3 ms | 4.5 ms | 100% | 0.4% |
| typo | flashfind | 1200 | 0.62 ms | 3.5 ms | 8.3 ms | 95% | 0.0% |
| typo | fff | 1200 | 22 ms | 39 ms | 56 ms | 86% | 0.5% |
| typo | fsearch | 1200 | **0.52 ms** | 2.2 ms | 4.4 ms | 98% | 0.8% |
| folder | flashfind | 289 | 3.3 ms | 9.4 ms | 16 ms | 99% | 0.0% |
| folder | fff | 289 | 5.7 ms | 24 ms | 38 ms | 92% | 0.4% |
| folder | fsearch | 289 | **1.7 ms** | 5.0 ms | 9.0 ms | 100% | 0.6% |
| keys | flashfind | 894 | **0.22 ms** | 2.5 ms | 4.5 ms | 98% | – |
| keys | fff | 894 | 17 ms | 30 ms | 37 ms | 100% | – |
| keys | fsearch | 894 | 0.39 ms | 2.4 ms | 4.7 ms | 100% | – |

| engine | entries indexed | ready, cold | ready, warm | memory |
|---|---|---|---|---|
| flashfind | 551,344 | 1.4 s | 3.6 ms | 74 MB |
| fff | 508,888 | 2.2 s | n/a (no saved index) | 207 MB |
| fsearch | 555,555 | 928 ms | 2.1 ms | 132 MB |

### Linux kernel (tree replica)

96,066 visible files (not hidden, not gitignored; 109,022 entries in total) · Apple M5 Pro (15 cores, 52 GB) · macOS 26.5.1 · load 32.5 before, 28.2 after · 1,789 name queries, 528 keystrokes · top 50 · warm

| class | engine | n | p50 | p95 | p99 | right file first | junk in top 10 |
|---|---|---|---|---|---|---|---|
| exact | flashfind | 300 | 0.19 ms | 0.34 ms | 0.52 ms | 100% | 0.0% |
| exact | fff | 300 | 1.0 ms | 1.8 ms | 2.3 ms | 100% | 0.1% |
| exact | fsearch | 300 | **0.14 ms** | 0.31 ms | 0.47 ms | 100% | 0.1% |
| typo | flashfind | 1200 | 0.19 ms | 0.33 ms | 0.49 ms | 95% | 0.0% |
| typo | fff | 1200 | 1.0 ms | 1.9 ms | 2.4 ms | 90% | 0.2% |
| typo | fsearch | 1200 | **0.13 ms** | 0.26 ms | 0.45 ms | 97% | 0.1% |
| folder | flashfind | 289 | **0.27 ms** | 0.46 ms | 0.71 ms | 100% | 0.0% |
| folder | fff | 289 | 0.61 ms | 2.1 ms | 2.9 ms | 96% | 0.1% |
| folder | fsearch | 289 | 0.38 ms | 0.71 ms | 0.91 ms | 100% | 0.2% |
| keys | flashfind | 528 | **78 µs** | 0.40 ms | 0.59 ms | 100% | – |
| keys | fff | 528 | 1.2 ms | 2.3 ms | 2.7 ms | 100% | – |
| keys | fsearch | 528 | 0.17 ms | 0.85 ms | 1.1 ms | 100% | – |

| engine | entries indexed | ready, cold | ready, warm | memory |
|---|---|---|---|---|
| flashfind | 102,116 | 154 ms | 0.55 ms | 33 MB |
| fff | 96,066 | 186 ms | n/a (no saved index) | 43 MB |
| fsearch | 109,022 | 99 ms | 0.51 ms | 26 MB |

### 200 synthetic repos

47,883 visible files (not hidden, not gitignored; 271,525 entries in total) · Apple M5 Pro (15 cores, 52 GB) · macOS 26.5.1 · load 28.2 before, 29.1 after · 1,800 name queries, 617 keystrokes, 300 repo: queries · top 50 · warm

| class | engine | n | p50 | p95 | p99 | right file first | junk in top 10 |
|---|---|---|---|---|---|---|---|
| exact | flashfind | 300 | **0.12 ms** | 0.22 ms | 0.29 ms | 100% | 0.0% |
| exact | fff | 300 | 1.1 ms | 2.2 ms | 2.9 ms | 100% | 5.2% |
| exact | fsearch | 300 | 0.22 ms | 0.35 ms | 0.72 ms | 100% | 42.7% |
| typo | flashfind | 1200 | **0.12 ms** | 0.21 ms | 0.31 ms | 98% | 0.0% |
| typo | fff | 1200 | 0.97 ms | 1.8 ms | 2.7 ms | 99% | 5.4% |
| typo | fsearch | 1200 | 0.20 ms | 0.33 ms | 0.55 ms | 99% | 28.8% |
| folder | flashfind | 300 | **0.19 ms** | 0.41 ms | 0.55 ms | 100% | 0.0% |
| folder | fff | 300 | 0.49 ms | 0.94 ms | 1.3 ms | 96% | 3.5% |
| folder | fsearch | 300 | 0.56 ms | 0.91 ms | 1.4 ms | 100% | 3.7% |
| keys | flashfind | 617 | **82 µs** | 0.29 ms | 0.38 ms | 100% | – |
| keys | fff | 617 | 0.85 ms | 1.3 ms | 1.5 ms | 100% | – |
| keys | fsearch | 617 | 0.23 ms | 0.83 ms | 1.0 ms | 100% | – |
| repo | flashfind | 300 | **100 µs** | 0.16 ms | 0.20 ms | 100% | – |

| engine | entries indexed | ready, cold | ready, warm | memory |
|---|---|---|---|---|
| flashfind | 30,566 | 113 ms | 0.65 ms | 13 MB |
| fff | 34,549 | 113 ms | n/a (no saved index) | 15 MB |
| fsearch | 271,525 | 478 ms | 0.78 ms | 64 MB |

