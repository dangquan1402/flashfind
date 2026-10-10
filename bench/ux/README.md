# bench/ux: does the file you meant come first?

`flashfind bench-ux` replays a history of "I wanted this file" tasks against
each ranking variant, cumulatively:

1. **0.1 baseline**: match score + the old additive repo-recency bonus.
2. **+ query memory (F1)**: picks remembered per query (and prefix queries).
3. **+ file frecency (F2)**: decayed picks + edits; repo bonus made mild.
4. **+ current project, dirty, recent commits (F3)**.
5. **+ ranked empty state (F4)**: suggestions before typing count as 0 keystrokes.

Each variant gets its own empty memory and replays the same tasks in time
order, recording the pick under the prefix it actually typed, so later tasks
benefit from earlier ones exactly as they would in use.

## Metrics

- **right file first**: the intended file is result #1 for the full query
  (the file's stem, e.g. `router` for `src/router.ts`).
- **mean rank / MRR** for the full query (rank capped at 51).
- **keystrokes to open**: the simulated user types the stem one character at
  a time and opens the file as soon as it is in the top 8, paying one
  keystroke per arrow press: min over k of (k + rank_k − 1). With F4, k = 0
  (picked from the suggestions) is allowed. Enter is not counted.
- **MCP → Read shown / right**: how often `find_files` would print the
  `→ Read <path>` hint for the full query, and how often that path is the
  intended file.

## Sources of tasks

- **Synthetic** (`--seed`): 80 repos with realistic name collisions (every
  repo has `README.md`, `config.*`, `index.ts`/`main.rs`/`__init__.py`, and
  14 files drawn from a shared vocabulary). A simulated user works 30 days in
  6 active repos (one "main" repo per day), opens 20 files a day from a
  per-repo 12-file working set (Zipf popularity, slow drift), mistypes 10% of
  queries, edits half the files it opens, and commits most repos' changes at
  the end of each day. The first 3 days are warm-up. Context = the repo of the
  previous task.
- **Real** (`--real`, run on the Mac): the 15 most active repos of the real
  index; every file changed in a non-bulk commit (≤20 files) in the last 60
  days is a task "open this file 10 minutes before the commit", in time
  order (max 1500, first 20% warm-up). Context = the repo of the previous
  task, recent commits = that repo's commits before the task, dirty = unknown
  (empty). It reads git logs only; it does not touch your memory file.

## Caveats (read before quoting numbers)

- The synthetic user is made up, and its habits (a small working set,
  repeated opens, working in one project at a time) are exactly what these
  features exploit. Treat synthetic gains as an upper bound.
- The real replay uses "files you committed" as a proxy for "files you
  looked for". Real file names there are long and distinctive (16 chars on
  average), so the baseline is already right most of the time; the gain
  comes from the ambiguous names (`README.md`, `lib.rs`, `index.ts`).
- Queries are always the file stem. People also type folder words, partial
  paths or `repo:` filters, which this doesn't model.
- The keystroke model assumes you notice the file the moment it's in the top
  8.

Run: `bench/ux/run.sh` (synthetic, 3 seeds) or `bench/ux/run.sh --real`.
