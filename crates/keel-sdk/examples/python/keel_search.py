"""Search every file with Keel from Python through keel_sdk and ctypes.

Needs keel-daemon running for the profile (`keel daemon start`) and the library built
(`cargo build -p keel-sdk --release`). Usage:

    python keel_search.py "invoice *.pdf" [--profile NAME] [--max 20] [--lib PATH]

The library is looked for with --lib, then the KEEL_SDK_LIB environment variable, then
target/release next to this repository. The SDK only searches: it never opens, runs,
changes or deletes a file.
"""

import argparse
import ctypes
import datetime
import os
import sys
import threading

KEEL_OK = 0
KEEL_SORT_NAME_ASCENDING = 1
KEEL_REQUEST_FULL_PATH_AND_FILE_NAME = 0x4
KEEL_REQUEST_SIZE = 0x10
KEEL_REQUEST_DATE_MODIFIED = 0x40
KEEL_REQUEST_ATTRIBUTES = 0x100
UNKNOWN_FILETIME = 0xFFFFFFFFFFFFFFFF

QUERY_CALLBACK = ctypes.CFUNCTYPE(None, ctypes.c_void_p, ctypes.c_uint32)


def library_path(given):
    if given:
        return given
    if os.environ.get("KEEL_SDK_LIB"):
        return os.environ["KEEL_SDK_LIB"]
    name = {"win32": "keel_sdk.dll", "darwin": "libkeel_sdk.dylib"}.get(
        sys.platform, "libkeel_sdk.so"
    )
    repo = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "..", ".."))
    return os.path.join(repo, "target", "release", name)


def load(path):
    lib = ctypes.CDLL(path)
    u32, c_int = ctypes.c_uint32, ctypes.c_int
    sigs = {
        "keel_set_search": ([ctypes.c_char_p], c_int),
        "keel_set_profile": ([ctypes.c_char_p], c_int),
        "keel_set_match_case": ([c_int], None),
        "keel_set_max": ([u32], None),
        "keel_set_sort": ([u32], c_int),
        "keel_set_request_flags": ([u32], None),
        "keel_query": ([c_int, QUERY_CALLBACK, ctypes.c_void_p], c_int),
        "keel_get_num_results": ([], u32),
        "keel_get_total_results": ([], u32),
        "keel_is_folder_result": ([u32], c_int),
        "keel_get_result_full_path_w": ([u32, ctypes.POINTER(ctypes.c_uint16), u32], u32),
        "keel_get_result_size": ([u32, ctypes.POINTER(ctypes.c_uint64)], c_int),
        "keel_get_result_date_modified": ([u32, ctypes.POINTER(ctypes.c_uint64)], c_int),
        "keel_get_result_attributes": ([u32], u32),
        "keel_get_last_error": ([], u32),
        "keel_get_last_error_message": ([ctypes.c_char_p, u32], u32),
        "keel_cleanup": ([], None),
    }
    for name, (args, res) in sigs.items():
        fn = getattr(lib, name)
        fn.argtypes = args
        fn.restype = res
    return lib


def error(lib):
    buf = ctypes.create_string_buffer(1024)
    lib.keel_get_last_error_message(buf, len(buf))
    return "error %d: %s" % (lib.keel_get_last_error(), buf.value.decode("utf-8", "replace"))


def full_path(lib, i):
    # UTF-16 here to show the _w variant; keel_get_result_full_path is the UTF-8 one.
    n = lib.keel_get_result_full_path_w(i, None, 0)
    buf = (ctypes.c_uint16 * (n + 1))()
    lib.keel_get_result_full_path_w(i, buf, n + 1)
    return bytes(buf)[: 2 * n].decode("utf-16-le")


def date(filetime):
    if filetime == UNKNOWN_FILETIME:
        return "unknown"
    t = filetime / 10_000_000 - 11_644_473_600
    return datetime.datetime.fromtimestamp(t).strftime("%Y-%m-%d %H:%M")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("search", nargs="?", default="")
    ap.add_argument("--profile")
    ap.add_argument("--max", type=int, default=20)
    ap.add_argument("--lib")
    args = ap.parse_args()

    lib = load(library_path(args.lib))
    if args.profile and not lib.keel_set_profile(args.profile.encode()):
        sys.exit(error(lib))
    if not lib.keel_set_search(args.search.encode("utf-8")):
        sys.exit(error(lib))
    lib.keel_set_max(args.max)
    lib.keel_set_sort(KEEL_SORT_NAME_ASCENDING)
    lib.keel_set_request_flags(
        KEEL_REQUEST_FULL_PATH_AND_FILE_NAME
        | KEEL_REQUEST_SIZE
        | KEEL_REQUEST_DATE_MODIFIED
        | KEEL_REQUEST_ATTRIBUTES
    )

    # Non-blocking: the callback runs on the SDK's thread when the daemon answered.
    done = threading.Event()
    status = {}

    def on_done(_user_data, code):
        status["code"] = code
        done.set()

    callback = QUERY_CALLBACK(on_done)  # keep a reference until it has been called
    if not lib.keel_query(0, callback, None):
        sys.exit(error(lib))
    done.wait()
    if status["code"] != KEEL_OK:
        sys.exit(error(lib))

    n = lib.keel_get_num_results()
    print("%d of %d results" % (n, lib.keel_get_total_results()))
    size, modified = ctypes.c_uint64(), ctypes.c_uint64()
    for i in range(n):
        lib.keel_get_result_size(i, ctypes.byref(size))
        lib.keel_get_result_date_modified(i, ctypes.byref(modified))
        kind = "d" if lib.keel_is_folder_result(i) else " "
        print(
            "%s %12d  %s  0x%04x  %s"
            % (
                kind,
                size.value,
                date(modified.value),
                lib.keel_get_result_attributes(i),
                full_path(lib, i),
            )
        )
    lib.keel_cleanup()


if __name__ == "__main__":
    main()
