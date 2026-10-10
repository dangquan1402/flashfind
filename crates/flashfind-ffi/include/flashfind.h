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

#ifdef __cplusplus
}
#endif
#endif
