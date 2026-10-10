/* C smoke test for the flashfind C ABI.
 *   cc -I crates/flashfind-ffi/include crates/flashfind-ffi/examples/smoke.c \
 *      target/release/libflashfind.a -o target/ffi-smoke && target/ffi-smoke <index> <query>
 */
#include <stdio.h>
#include <stdlib.h>
#include "flashfind.h"

static void on_page(void *ctx, const ff_page *p) {
    (void)ctx;
    printf("%zu hits (total %llu%s, strategy %u, %llu us)\n", p->count, (unsigned long long)p->total,
           p->total_exact ? "" : "+", p->strategy, (unsigned long long)p->elapsed_us);
    for (size_t i = 0; i < p->count && i < 5; i++) printf("  %6d %s\n", p->hits[i].score, p->hits[i].path);
}

int main(int argc, char **argv) {
    if (argc < 3) { fprintf(stderr, "usage: %s <index.ffx> <query>\n", argv[0]); return 2; }
    char *err = NULL;
    ff_index *idx = ff_index_open(argv[1], &err);
    if (!idx) { fprintf(stderr, "open: %s\n", err); ff_string_free(err); return 1; }
    printf("flashfind %s, %llu entries\n", ff_version(), (unsigned long long)ff_index_entries(idx));
    ff_session *s = ff_session_new(idx);
    ff_cancel *c = ff_cancel_new();
    int rc = ff_search(s, argv[2], 10, c, on_page, NULL, &err);
    if (rc != FF_OK) { fprintf(stderr, "search rc=%d %s\n", rc, err ? err : ""); return 1; }
    ff_cancel_cancel(c);
    rc = ff_search(s, argv[2], 10, c, on_page, NULL, &err);
    printf("after cancel: rc=%d (expect %d)\n", rc, FF_CANCELLED);
    ff_cancel_free(c);
    ff_session_free(s);
    ff_index_free(idx);
    return rc == FF_CANCELLED ? 0 : 1;
}
