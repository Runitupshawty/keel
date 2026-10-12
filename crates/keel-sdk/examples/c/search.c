/*
 * search.c: search every file with Keel from C through keel_sdk.
 *
 * Build (the library from `cargo build -p keel-sdk --release`, in target/release):
 *   Windows (MSVC): cl /I crates\keel-sdk\include crates\keel-sdk\examples\c\search.c
 *                   target\release\keel_sdk.dll.lib
 *   Linux / macOS:  cc -I crates/keel-sdk/include crates/keel-sdk/examples/c/search.c
 *                   -Ltarget/release -lkeel_sdk -o search
 * Run with keel-daemon running for the profile (`keel daemon start`):
 *   search "invoice *.pdf" [profile]
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#include "keel_sdk.h"

/* FILETIME (100 ns since 1601) to Unix seconds. */
static time_t unix_time(uint64_t filetime) {
    return (time_t)(filetime / 10000000u - 11644473600u);
}

static void fail(const char *what) {
    char message[512];
    keel_get_last_error_message(message, sizeof message);
    fprintf(stderr, "%s failed: error %u: %s\n", what, (unsigned)keel_get_last_error(), message);
    keel_cleanup();
    exit(1);
}

int main(int argc, char **argv) {
    const char *search = argc > 1 ? argv[1] : "";
    if (argc > 2 && !keel_set_profile(argv[2])) {
        fail("keel_set_profile");
    }
    if (!keel_set_search(search)) {
        fail("keel_set_search");
    }
    keel_set_max(20);
    keel_set_sort(KEEL_SORT_DATE_MODIFIED_DESCENDING);
    keel_set_request_flags(KEEL_REQUEST_FULL_PATH_AND_FILE_NAME | KEEL_REQUEST_SIZE |
                           KEEL_REQUEST_DATE_MODIFIED);

    /* Blocking: returns when the daemon answered. */
    if (!keel_query(1, NULL, NULL)) {
        fail("keel_query");
    }

    uint32_t shown = keel_get_num_results();
    printf("%u of %u results\n", (unsigned)shown, (unsigned)keel_get_total_results());
    for (uint32_t i = 0; i < shown; i++) {
        /* Ask for the length first, then copy. */
        uint32_t len = keel_get_result_full_path(i, NULL, 0);
        char *path = malloc((size_t)len + 1);
        if (path == NULL) {
            break;
        }
        keel_get_result_full_path(i, path, len + 1);

        uint64_t size = 0, modified = 0;
        keel_get_result_size(i, &size);
        keel_get_result_date_modified(i, &modified);
        char date[32] = "unknown";
        if (modified != UINT64_MAX) {
            time_t t = unix_time(modified);
            struct tm *tm = localtime(&t);
            if (tm != NULL) {
                strftime(date, sizeof date, "%Y-%m-%d %H:%M", tm);
            }
        }
        printf("%s%12" PRIu64 "  %s  %s\n", keel_is_folder_result(i) ? "d" : " ", size, date,
               path);
        free(path);
    }
    keel_cleanup();
    return 0;
}
