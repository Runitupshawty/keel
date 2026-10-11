# Fuzzing

Keel reads bytes it did not write: archives, photos, SSH configs, search queries, JSON-RPC
and MCP messages, peers' keel-net messages, shared uploads, files it previews. Each such
parser has a fuzz target: a function that must return, with a result or an error, for any
input. It must never panic, never allocate by a size the input only declares (a header's
claim instead of the bytes that are there), never read outside the input and never take
unbounded time.

The targets run two ways:

- **In `cargo test`** (every platform, every CI run). Each target runs on its corpus
  (`fuzz/corpus/<target>/`) and then on seeded mutations of it: 2,000 cases per target, 500
  under CI (`CI` set), or `KEEL_FUZZ_CASES`. The mutations are seeded by the target's name,
  so a failure reproduces. A case that panics or runs over 10 s fails the test and is saved
  to `target/fuzz-failures/<target>-<hash>`; the test binaries' allocator refuses any single
  allocation over 512 MiB, so a size taken from a hostile header aborts the run. The
  harness is `keel_vfs::fuzz_harness`.
- **With libFuzzer** (`fuzz/`, Linux or macOS, nightly Rust; libFuzzer does not build for
  MSVC Windows). Coverage-guided, with AddressSanitizer and overflow checks. CI runs every
  target for 60 s on each push to `main` (the `fuzz` job, not part of `check`).

Both call the same entry points: `fuzz::*` in each crate, compiled for tests and with the
crate's `fuzzing` feature.

## Targets

| Target | Entry point | What it reads |
|---|---|---|
| `archive` | `keel_vfs::fuzz::archive` | zip (zip64), tar, tar.gz/.bz2/.xz/.zst, 7z, RAR: listing, every body, and the zip rewriter deleting, renaming and adding entries |
| `ssh_config` | `keel_vfs::fuzz::ssh_config` | `~/.ssh/config` with `Include`s, resolved for the aliases it names |
| `tiff` | `keel_core::fuzz::tiff` | a TIFF's orientation (first IFD) and metadata |
| `raw` | `keel_core::fuzz::raw` | camera RAW files: the walk of IFDs and SubIFDs to the embedded JPEG previews (bounded in count and depth), metadata, small previews |
| `image_meta` | `keel_core::fuzz::image_meta` | photo metadata: EXIF, XMP, dimensions, small thumbnails |
| `probe` | `keel_core::fuzz::probe` | ffprobe JSON as the video metadata reader takes it |
| `query` | `keel_core::fuzz::query` | the library search box (`LibraryQuery::parse`, media filters) and the SQL it becomes |
| `rpc` | `keel_api::fuzz::rpc` | JSON-RPC lines from the daemon socket, each request's params decoded by its operation |
| `mcp` | `keel_api::fuzz::mcp` | an MCP stdio session, with and without `--allow-execute` |
| `wire` | `keel_net::fuzz::wire` | keel-net messages (CBOR): requests, Spacedrop offers, sync pages (applied to a library), pairing tickets |
| `pair_code` | `keel_net::fuzz::pair_code` | pairing codes and tickets as typed or scanned, device ids |
| `multipart` | `keel_daemon::fuzz::multipart` | a Web Share Target upload: `Content-Type` line, then the `multipart/form-data` body |
| `preview` | `keel_preview::fuzz::preview` | the preview panel: the first byte picks the type (`keel_preview::fuzz::EXTENSIONS`), the rest is the file (text in any encoding, CSV, xlsx/xls/ods, docx, pptx, OpenDocument, SVG, images) |
| `font` | `keel_preview::fuzz::font` | font files (TrueType, OpenType, collections): the name table and the sample drawn in the font, with glyph boxes checked before they are rasterized |

keel-app is a binary, so its parsers (the Linux clipboard's `text/uri-list` and
`x-special/gnome-copied-files`, `.desktop` files and `mimeapps.list`, icon theme SVG checks,
.vsix packages and VS Code color theme JSON) run only in `cargo test`
(`crates/keel-app/src/fuzz_tests.rs`, targets `clipboard`, `desktop_entry`, `icon_svg`,
`vsix`, `color_theme`). PDF and video previews hand the file to
pdfium and ffmpeg and are not fuzzed here.

## Running libFuzzer

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
cargo +nightly fuzz build -a                  # -a: debug assertions and overflow checks
cargo +nightly fuzz run -a archive fuzz/corpus/archive -- -max_total_time=300 -max_len=65536
```

New interesting inputs go to the first corpus folder given; crashes and timeouts to
`fuzz/artifacts/<target>/`. Reproduce one with
`cargo +nightly fuzz run -a <target> fuzz/artifacts/<target>/crash-…`, or put it in
`fuzz/corpus/<target>/` and run `cargo test -p <crate> --lib fuzz` (`--bins fuzz_tests` for
keel-app) on any platform.

`fuzz/` is not a member of the Keel workspace (it needs nightly); it has its own
`Cargo.lock`, kept in step with the workspace's.

## Corpus policy

`fuzz/corpus/<target>/` holds small inputs only (a few KiB; none over 64 KiB): hand-written
or generated seeds that reach each format's features, hostile inputs (huge declared sizes
and counts, loops, deep nesting, edge numbers), and every input that once made a target
fail, so `cargo test` checks it from then on. Never real documents, photos or archives of
anyone's: the corpus is public. Do not commit libFuzzer's whole grown corpus; minimise
first (`cargo +nightly fuzz cmin <target>`) and keep what adds coverage. The files are
bytes (`fuzz/corpus/.gitattributes`: no line-ending conversion).

A fix for a fuzz finding comes with its input in the corpus and, where the cause is clear
from it, a unit test next to the fixed code.

libfuzzer-sys aborts on every panic, even one that is caught. The `preview` target puts the
default panic hook back (its previewers catch docx-rs's panics on damaged files, which is
the intended behaviour); a panic that reaches a target still aborts it.

## First runs (2026-10-10)

Each libFuzzer target ran with `-a`, ASan, `-fork=2`, `-timeout=10`, `-malloc_limit_mb=512`
on an 8-thread Linux machine, four targets at a time: about 4 minutes each, then 5
more minutes after the fixes (`preview`: 5 minutes more after its panic-hook change). The
second round found nothing new.

| Target | Executions (round 1 / round 2) | Found |
|---|---|---|
| `archive` | 139k / 138k | nothing (the zip64 size overflow was found by a hand-written corpus input) |
| `ssh_config` | 274k / 321k | nothing |
| `tiff` | 342k / 391k | nothing |
| `image_meta` | 270k / 269k | 5 inputs: a GIF frame larger than its screen (860 MB allocation) |
| `probe` | 5.6M / 5.1M | nothing |
| `query` | 1.1M / 1.1M | 10 inputs: integer overflow in size, pixel, duration and date filters |
| `rpc` | 78k / 72k | nothing |
| `mcp` | 120k / 107k | nothing |
| `wire` | 469k / 587k | nothing |
| `pair_code` | 4.1M / 4.2M | nothing |
| `multipart` | 560k / 537k | nothing |
| `preview` | 31k / 12k + 50k | 3 inputs: docx-rs panics on damaged .docx files |

The in-tree runs found the same query overflow, the GIF allocation and the docx panics on
their own.
