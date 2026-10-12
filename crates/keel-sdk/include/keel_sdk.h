/*
 * keel_sdk.h: search every file on this computer with Keel from C (or anything with a C
 * FFI), in the shape of Everything's SDK. The library (keel_sdk.dll, libkeel_sdk.so,
 * libkeel_sdk.dylib) talks JSON-RPC to the user's running keel-daemon over its per-user
 * local socket and runs the `system.search` operation: it searches and returns results
 * only, and never opens, runs, changes or deletes a file.
 *
 * Usage: set the search and its switches, call keel_query, read the results by index.
 * All functions are thread-safe and share one process-wide state (as Everything's SDK).
 * Booleans are int (0 false, anything else true). Strings are NUL-ended UTF-8 (char *) or
 * UTF-16 (uint16_t *, a wchar_t * on Windows).
 *
 * String getters copy into the caller's buffer of `len` units, cut at a character boundary
 * to fit with the NUL, and return the length the whole string needs without the NUL: call
 * with out = NULL to learn it. They return 0 on error (see keel_get_last_error).
 *
 * See docs/api.md, "SDK". Licensed MIT OR Apache-2.0.
 */
#ifndef KEEL_SDK_H
#define KEEL_SDK_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && !defined(KEEL_SDK_STATIC)
#define KEEL_API __declspec(dllimport)
#else
#define KEEL_API
#endif

/* Error codes (keel_get_last_error); Everything's numbers where the meaning is the same. */
#define KEEL_OK 0
/* Out of memory. */
#define KEEL_ERROR_MEMORY 1
/* keel-daemon is not running for the profile, or the connection to it failed. */
#define KEEL_ERROR_IPC 2
/* A background query's thread could not be started. */
#define KEEL_ERROR_CREATETHREAD 5
/* The result index is out of range. */
#define KEEL_ERROR_INVALIDINDEX 6
/* No query has finished since the last reset (no results to read). */
#define KEEL_ERROR_INVALIDCALL 7
/* The property was not requested with keel_set_request_flags before the query. */
#define KEEL_ERROR_INVALIDREQUEST 8
/* A bad argument: text not UTF-8/UTF-16 or over 4096 bytes, a bad regular expression, an
   unknown sort or profile name, a NULL output pointer. */
#define KEEL_ERROR_INVALIDPARAMETER 9
/* Keel has no such data (run counts), or the daemon is too old for system.search. */
#define KEEL_ERROR_UNSUPPORTED 10
/* The daemon's search failed (keel_get_last_error_message says why). */
#define KEEL_ERROR_SEARCH 11
/* The daemon did not answer in time. */
#define KEEL_ERROR_TIMEOUT 12
/* A newer query, keel_reset or keel_cleanup replaced this query; its results were dropped. */
#define KEEL_ERROR_CANCELLED 13

/* Sorts (keel_set_sort): Everything's numbers for the four orders Keel has. */
#define KEEL_SORT_NAME_ASCENDING 1
#define KEEL_SORT_NAME_DESCENDING 2
#define KEEL_SORT_PATH_ASCENDING 3
#define KEEL_SORT_PATH_DESCENDING 4
#define KEEL_SORT_SIZE_ASCENDING 5
#define KEEL_SORT_SIZE_DESCENDING 6
#define KEEL_SORT_DATE_MODIFIED_ASCENDING 13
#define KEEL_SORT_DATE_MODIFIED_DESCENDING 14

/* Request flags (keel_set_request_flags): Everything's bits. Default: file name and path. */
#define KEEL_REQUEST_FILE_NAME 0x00000001u
#define KEEL_REQUEST_PATH 0x00000002u
#define KEEL_REQUEST_FULL_PATH_AND_FILE_NAME 0x00000004u
#define KEEL_REQUEST_SIZE 0x00000010u
#define KEEL_REQUEST_DATE_MODIFIED 0x00000040u
#define KEEL_REQUEST_ATTRIBUTES 0x00000100u
/* Accepted, but Keel keeps no run counts: the run count functions fail with
   KEEL_ERROR_UNSUPPORTED. */
#define KEEL_REQUEST_RUN_COUNT 0x00000400u

/* What keel_get_result_attributes returns on error (Windows' INVALID_FILE_ATTRIBUTES). */
#define KEEL_INVALID_FILE_ATTRIBUTES 0xFFFFFFFFu

/* A background query ended with `error` (KEEL_OK: its results are ready). Called on the
   query's own thread. */
typedef void (*keel_query_callback)(void *user_data, uint32_t error);

/* --- The search (applies to the next keel_query) --- */

/* Words, "phrases", wildcards, or with keel_set_regex one regular expression. NULL: the
   empty search (everything). 0 for text not UTF-8 or over 4096 bytes. */
KEEL_API int keel_set_search(const char *search);
KEEL_API int keel_set_search_w(const uint16_t *search);
KEEL_API void keel_set_match_case(int on);
/* Words match whole words of the name only. */
KEEL_API void keel_set_match_whole_word(int on);
/* Words match the full path, not only the name. */
KEEL_API void keel_set_match_path(int on);
/* Accents count (an accented letter does not match the plain one). */
KEEL_API void keel_set_match_diacritics(int on);
KEEL_API void keel_set_regex(int on);
/* Results kept at most (default 0xFFFFFFFF: all; 0 counts only). */
KEEL_API void keel_set_max(uint32_t max);
/* Results skipped first (default 0). */
KEEL_API void keel_set_offset(uint32_t offset);
/* A KEEL_SORT_* value; 0 (sort unchanged) for another. Default name ascending. */
KEEL_API int keel_set_sort(uint32_t sort);
/* KEEL_REQUEST_* bits: what the next query's results can be read for. */
KEEL_API void keel_set_request_flags(uint32_t flags);
/* The keel-daemon profile to search through (NULL or "": "default"); 0 for a bad name.
   The daemon's folders come from KEEL_CONFIG_DIR / KEEL_DATA_DIR when set. */
KEEL_API int keel_set_profile(const char *profile);

/* --- Querying --- */

/* wait != 0: returns when done, 1 on success (callback, when given, is also called before
   returning). wait == 0: runs on a new thread and returns 1 once it started; callback is
   required and is called on that thread when the query ends. The results replace the
   previous ones only when the query succeeded and is still the latest. */
KEEL_API int keel_query(int wait, keel_query_callback callback, void *user_data);

/* --- Results of the last query --- */

/* Results held (after offset and max). */
KEEL_API uint32_t keel_get_num_results(void);
/* Matches in all, regardless of offset and max (counted over the first 10000 found). */
KEEL_API uint32_t keel_get_total_results(void);
KEEL_API int keel_is_folder_result(uint32_t index);
/* Needs KEEL_REQUEST_FULL_PATH_AND_FILE_NAME, or both FILE_NAME and PATH. */
KEEL_API uint32_t keel_get_result_full_path(uint32_t index, char *out, uint32_t len);
KEEL_API uint32_t keel_get_result_full_path_w(uint32_t index, uint16_t *out, uint32_t len);
/* Needs KEEL_REQUEST_FILE_NAME. */
KEEL_API uint32_t keel_get_result_file_name(uint32_t index, char *out, uint32_t len);
KEEL_API uint32_t keel_get_result_file_name_w(uint32_t index, uint16_t *out, uint32_t len);
/* The folder the result is in. Needs KEEL_REQUEST_PATH. */
KEEL_API uint32_t keel_get_result_path(uint32_t index, char *out, uint32_t len);
KEEL_API uint32_t keel_get_result_path_w(uint32_t index, uint16_t *out, uint32_t len);
/* Bytes (0 for a folder) into *size; 1 on success. Needs KEEL_REQUEST_SIZE. */
KEEL_API int keel_get_result_size(uint32_t index, uint64_t *size);
/* A FILETIME (100 ns units since 1601-01-01 UTC; a Windows FILETIME * may be cast) into
   *filetime, 0xFFFFFFFFFFFFFFFF when unknown; 1 on success. Needs
   KEEL_REQUEST_DATE_MODIFIED. */
KEEL_API int keel_get_result_date_modified(uint32_t index, uint64_t *filetime);
/* FILE_ATTRIBUTE_* bits (read from the file system on Windows; folder or normal, plus
   hidden for dot names, elsewhere), or KEEL_INVALID_FILE_ATTRIBUTES. Needs
   KEEL_REQUEST_ATTRIBUTES. */
KEEL_API uint32_t keel_get_result_attributes(uint32_t index);

/* Unsupported: Keel keeps no run counts. Each returns 0 with KEEL_ERROR_UNSUPPORTED. */
KEEL_API uint32_t keel_get_result_run_count(uint32_t index);
KEEL_API uint32_t keel_get_run_count(const char *path);
KEEL_API uint32_t keel_inc_run_count(const char *path);

/* --- State --- */

/* The last KEEL_ERROR_* code (process-wide); KEEL_OK after a query that succeeded. */
KEEL_API uint32_t keel_get_last_error(void);
/* The last error's message in UTF-8 (empty when none). */
KEEL_API uint32_t keel_get_last_error_message(char *out, uint32_t len);
/* Default settings (the profile stays), no results, no error. */
KEEL_API void keel_reset(void);
/* As keel_reset, and the profile back to "default". Wait for a running background query's
   callback before unloading the library. */
KEEL_API void keel_cleanup(void);

#ifdef __cplusplus
}
#endif

#endif /* KEEL_SDK_H */
