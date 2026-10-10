/* flashfind C ABI – in-process file name search.
 *
 * Lifetime: ff_index is immutable and thread-safe; ff_session serialises
 * its own searches; ff_cancel may be cancelled from any thread.
 * Typical per-keystroke use from an app:
 *
 *   ff_cancel_cancel(prev); prev = ff_cancel_new();
 *   ff_search(session, text, 50, prev, on_page, ctx, &err);   // on a worker
 *
 * Strings returned through `err` must be released with ff_string_free.
 */
#ifndef FLASHFIND_H
#define FLASHFIND_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct FfIndex ff_index;
typedef struct FfSession ff_session;
typedef struct FfCancel ff_cancel;

typedef struct {
    const char *path;   /* absolute, NUL-terminated, valid during the callback only */
    uint32_t path_len;
    uint8_t kind;       /* 0 file, 1 dir, 2 symlink, 3 other */
    int32_t score;
    uint64_t size;
    int64_t mtime;      /* Unix seconds */
    uint32_t entry;
} ff_hit;

typedef struct {
    const ff_hit *hits;
    size_t count;
    uint64_t total;      /* matching entries; lower bound if !total_exact */
    uint8_t total_exact;
    uint8_t strategy;    /* 0 full, 1 narrowed, 2 cached, 3 prefix-range, 4 entry-scan */
    uint64_t elapsed_us;
} ff_page;

typedef void (*ff_page_cb)(void *ctx, const ff_page *page);

#define FF_OK 0
#define FF_CANCELLED 1
#define FF_ERROR (-1)

const char *ff_version(void);

ff_index *ff_index_open(const char *path, char **err);
ff_index *ff_index_build(const char *root, uint32_t threads, char **err);
int32_t ff_index_save(const ff_index *idx, const char *path, char **err);
uint64_t ff_index_entries(const ff_index *idx);
void ff_index_free(ff_index *idx);

ff_session *ff_session_new(const ff_index *idx);
void ff_session_free(ff_session *s);

ff_cancel *ff_cancel_new(void);
void ff_cancel_cancel(const ff_cancel *c);
void ff_cancel_free(ff_cancel *c);

/* Delivers the first page (top `limit`, 0 = default) through `cb` once and
 * returns FF_OK; FF_CANCELLED without calling `cb`; FF_ERROR with *err. */
int32_t ff_search(ff_session *s, const char *query, uint32_t limit, const ff_cancel *cancel,
                  ff_page_cb cb, void *ctx, char **err);

void ff_string_free(char *s);

/* ---- Live index: base index + in-memory overlay of changes ---------- */
typedef struct FfLive ff_live;

typedef struct {
    uint8_t phase;          /* 0 crawling, 1 building, 2 ready */
    uint8_t watching;
    uint64_t entries;       /* crawled so far (crawling) / in the base index */
    uint64_t dirs;
    uint64_t overlay;       /* entries added/changed since the base index */
    uint64_t dead;          /* base entries removed or replaced */
    uint64_t last_event_id; /* newest FSEvents id applied */
    uint64_t generation;    /* bumps when compaction swaps the base */
    uint64_t compactions;
    double elapsed_ms;
} ff_progress;

/* Open a saved index for live use; compactions are saved back to it. */
ff_live *ff_live_open(const char *index_path, char **err);
/* Crawl in the background; searchable at once (partial results while
   phase < 2). index_path may be NULL (memory only). */
ff_live *ff_live_crawl(const char *root, const char *index_path, uint32_t threads, char **err);
/* Watch for changes: FSEvents on macOS (replays events since the saved
   index), mtime polling elsewhere. */
int ff_live_watch(const ff_live *live, char **err);
void ff_live_unwatch(const ff_live *live);
/* Relist one directory now. */
int ff_live_refresh(const ff_live *live, const char *dir);
void ff_live_progress(const ff_live *live, ff_progress *out);
/* 1 if compacted, 0 if nothing to do. */
int ff_live_compact(const ff_live *live);
/* Compact if dirty, else record the last event id. Call before quitting. */
void ff_live_checkpoint(const ff_live *live);
/* Search session over the live index (use with ff_search). */
ff_session *ff_live_session_new(const ff_live *live);
void ff_live_free(ff_live *live);

/* ---- Code projects: git repos under your roots, gitignore-aware ------ */
typedef struct FfCode ff_code;
typedef struct FfCodeSession ff_code_session;

typedef struct {
    const char *path;       /* absolute */
    const char *rel;        /* relative to the repo root */
    const char *repo;       /* repo (folder) name */
    const char *repo_root;  /* absolute repo root */
    uint32_t group;         /* index of the repo group in display order */
    uint8_t kind;           /* 0 file, 1 dir, 2 symlink, 3 other */
    int32_t score;
    uint64_t size;
    int64_t mtime;
    int64_t repo_active;    /* repo's last activity, Unix seconds */
} ff_code_hit;

typedef struct {
    const ff_code_hit *hits; /* grouped by repo, best group first */
    size_t count;
    uint64_t total;
    uint32_t groups;
    uint64_t elapsed_us;
} ff_code_page;

typedef void (*ff_code_page_cb)(void *ctx, const ff_code_page *page);

typedef struct {
    uint64_t repos, entries, overlays, removed, events, rewalks;
    uint64_t repos_added, repos_removed, compactions, last_event_id;
    uint8_t watching;
    double last_apply_ms;
} ff_code_stats;

/* roots: ':'-separated or NULL ($FLASHFIND_CODE_ROOTS or ~); cache_dir:
   NULL for ~/.cache/flashfind. May take seconds the first time: call off
   the main thread. */
ff_code *ff_code_open(const char *roots, const char *cache_dir, char **err);
int32_t ff_code_watch(const ff_code *c, char **err);   /* FSEvents on macOS */
void ff_code_unwatch(const ff_code *c);
void ff_code_get_stats(const ff_code *c, ff_code_stats *out);
char *ff_code_projects_json(const ff_code *c, const char *filter, uint32_t limit); /* ff_string_free */
void ff_code_checkpoint(const ff_code *c);
void ff_code_free(ff_code *c);
ff_code_session *ff_code_session_new(const ff_code *c);
void ff_code_session_free(ff_code_session *s);
/* Query language + repo:/lang:/dirty:. Filters only => files newest first.
   An empty query delivers an empty page. */
int32_t ff_code_search(ff_code_session *s, const char *query, uint32_t limit, const ff_cancel *cancel,
                       ff_code_page_cb cb, void *ctx, char **err);

#ifdef __cplusplus
}
#endif
#endif
