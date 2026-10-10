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
- **Machine load was very high** during these runs (load averages 27–114 from
  unrelated work: Xcode builds, a simulator), so absolute numbers are noisy and
  p95/p99 especially. Ratios are more meaningful than absolutes. Re-run on a
  quiet machine before quoting.
- **Entry counts differ.** flashfind and fsearch count files plus folders. fff
  counts files only. fsearch also indexes ignored and junk files.

### Chromium (tree replica)

507,278 visible files (not hidden, not gitignored; 555,555 entries in total) · Apple M5 Pro (15 cores, 52 GB) · macOS 26.5.1 · load 27.1 before, 113.8 after · 1,789 name queries, 894 keystrokes · top 50 · warm

| class | engine | n | p50 | p95 | p99 | right file first | junk in top 10 |
|---|---|---|---|---|---|---|---|
| exact | flashfind | 300 | 3.3 ms | 14 ms | 24 ms | 99% | 0.0% |
| exact | fff | 300 | 35 ms | 69 ms | 112 ms | 100% | 0.6% |
| exact | fsearch | 300 | **1.1 ms** | 6.8 ms | 15 ms | 100% | 0.4% |
| typo | flashfind | 1200 | 2.9 ms | 14 ms | 25 ms | 95% | 0.0% |
| typo | fff | 1200 | 34 ms | 67 ms | 101 ms | 86% | 0.5% |
| typo | fsearch | 1200 | **1.0 ms** | 8.1 ms | 21 ms | 98% | 0.8% |
| folder | flashfind | 289 | 11 ms | 29 ms | 36 ms | 99% | 0.0% |
| folder | fff | 289 | 10 ms | 43 ms | 59 ms | 92% | 0.4% |
| folder | fsearch | 289 | **2.7 ms** | 20 ms | 33 ms | 100% | 0.6% |
| keys | flashfind | 894 | **0.25 ms** | 11 ms | 20 ms | 98% | – |
| keys | fff | 894 | 32 ms | 61 ms | 86 ms | 100% | – |
| keys | fsearch | 894 | 0.96 ms | 15 ms | 30 ms | 100% | – |

| engine | entries indexed | ready, cold | ready, warm | memory |
|---|---|---|---|---|
| flashfind | 551,344 | 4.9 s | 33 ms | 204 MB |
| fff | 508,888 | 4.8 s | n/a (no saved index) | 209 MB |
| fsearch | 555,555 | 1.5 s | 15 ms | 126 MB |
### Linux kernel (tree replica)

96,066 visible files (not hidden, not gitignored; 109,022 entries in total) · Apple M5 Pro (15 cores, 52 GB) · macOS 26.5.1 · load 113.8 before, 90.1 after · 1,789 name queries, 528 keystrokes · top 50 · warm

| class | engine | n | p50 | p95 | p99 | right file first | junk in top 10 |
|---|---|---|---|---|---|---|---|
| exact | flashfind | 300 | 0.25 ms | 0.95 ms | 1.6 ms | 100% | 0.0% |
| exact | fff | 300 | 1.9 ms | 4.2 ms | 7.6 ms | 100% | 0.1% |
| exact | fsearch | 300 | **0.19 ms** | 0.67 ms | 1.5 ms | 100% | 0.1% |
| typo | flashfind | 1200 | 0.25 ms | 1.1 ms | 3.1 ms | 95% | 0.0% |
| typo | fff | 1200 | 1.8 ms | 4.2 ms | 7.2 ms | 90% | 0.2% |
| typo | fsearch | 1200 | **0.18 ms** | 0.67 ms | 2.0 ms | 97% | 0.1% |
| folder | flashfind | 289 | 0.56 ms | 1.4 ms | 2.1 ms | 100% | 0.0% |
| folder | fff | 289 | 1.1 ms | 4.5 ms | 7.4 ms | 96% | 0.1% |
| folder | fsearch | 289 | **0.53 ms** | 1.5 ms | 2.5 ms | 100% | 0.2% |
| keys | flashfind | 528 | **0.12 ms** | 0.61 ms | 0.86 ms | 100% | – |
| keys | fff | 528 | 1.8 ms | 3.6 ms | 4.4 ms | 100% | – |
| keys | fsearch | 528 | 0.22 ms | 1.2 ms | 2.1 ms | 100% | – |

| engine | entries indexed | ready, cold | ready, warm | memory |
|---|---|---|---|---|
| flashfind | 102,116 | 408 ms | 1.5 ms | 24 MB |
| fff | 96,066 | 369 ms | n/a (no saved index) | 42 MB |
| fsearch | 109,022 | 158 ms | 2.1 ms | 27 MB |
### 200 synthetic repos

47,883 visible files (not hidden, not gitignored; 271,525 entries in total) · Apple M5 Pro (15 cores, 52 GB) · macOS 26.5.1 · load 83.7 before, 61.4 after · 1,800 name queries, 617 keystrokes, 300 repo: queries · top 50 · warm

| class | engine | n | p50 | p95 | p99 | right file first | junk in top 10 |
|---|---|---|---|---|---|---|---|
| exact | flashfind | 300 | **0.14 ms** | 0.50 ms | 0.90 ms | 100% | 0.0% |
| exact | fff | 300 | 1.9 ms | 4.6 ms | 7.4 ms | 100% | 5.2% |
| exact | fsearch | 300 | 0.34 ms | 0.94 ms | 2.5 ms | 100% | 42.7% |
| typo | flashfind | 1200 | **0.12 ms** | 0.45 ms | 0.97 ms | 98% | 0.0% |
| typo | fff | 1200 | 1.7 ms | 4.1 ms | 8.6 ms | 99% | 5.4% |
| typo | fsearch | 1200 | 0.30 ms | 0.97 ms | 2.8 ms | 99% | 28.8% |
| folder | flashfind | 300 | **0.53 ms** | 1.5 ms | 1.7 ms | 100% | 0.0% |
| folder | fff | 300 | 0.86 ms | 2.5 ms | 4.8 ms | 96% | 3.5% |
| folder | fsearch | 300 | 0.89 ms | 2.4 ms | 4.3 ms | 100% | 3.7% |
| keys | flashfind | 617 | **0.19 ms** | 0.90 ms | 1.7 ms | 100% | – |
| keys | fff | 617 | 2.2 ms | 5.8 ms | 13 ms | 100% | – |
| keys | fsearch | 617 | 0.57 ms | 4.5 ms | 7.7 ms | 100% | – |
| repo | flashfind | 300 | **0.16 ms** | 0.70 ms | 0.98 ms | 100% | – |

| engine | entries indexed | ready, cold | ready, warm | memory |
|---|---|---|---|---|
| flashfind | 30,566 | 168 ms | 0.64 ms | 19 MB |
| fff | 34,549 | 129 ms | n/a (no saved index) | 15 MB |
| fsearch | 271,525 | 618 ms | 0.99 ms | 65 MB |
