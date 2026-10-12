# Fuzzing

Keel reads bytes it did not write: archives, photos, SSH configs, FTP listings, search queries, JSON-RPC
and MCP messages, peers' keel-net messages, shared uploads, files it previews. Each such
parser has a fuzz target: a function that must return, with a result or an error, for any
input. It must never panic, never allocate by a size the input only declares (a header's
claim instead of the bytes that are there), never read outside the input and never take
unbounded time.

The targets run two ways:

- **In `cargo test`** (every platform, every CI run). Each target runs on its corpus
  (`fuzz/corpus/<target>/`) and then on seeded mutations of it: 2,000 cases per target, 500
  under CI (`CI` set), or `KEEL_FUZZ_CASES`. The mutations are seeded by the target's name,
  so a failure reproduces. A case that panics or runs over 10 s (30 s under CI) fails the test and is saved
  to `target/fuzz-failures/<target>-<hash>`; the test binaries' allocator refuses any single
  allocation over 512 MiB, so a size taken from a hostile header aborts the run. The
  harness is `keel_vfs::fuzz_harness`; keel-query, which does not depend on keel-vfs, runs
  `query_parse` with a small copy of its loop (corpus, seeded mutations, time limit; no allocation
  cap and no saved failure file: the failing input is in the panic message).
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
| `du_scan_file` | `keel_vfs::fuzz::du_scan_file` | a disk usage scan file (gzip or plain JSON): the structure checks (parents first, siblings together, kinds in their places, indexes in range), then saved and loaded again to the same items |
| `ftp_list` | `keel_vfs::fuzz::ftp_list` | an FTP `LIST` reply: bounded lines (8 KiB, a million entries), each parsed as a UNIX `ls -l` or a DOS (IIS) line |
| `ftp_mlsd` | `keel_vfs::fuzz::ftp_mlsd` | an FTP `MLSD` reply and `MLST` answers: bounded lines of facts (type, size, modify) and names |
| `recover_ntfs` | `keel_vfs::fuzz::recover_ntfs` | deleted-file recovery on an NTFS volume image: the boot sector, `$MFT` records (update sequence fixups, attributes, data runs), `$Bitmap`, folder paths, then each file's data |
| `recover_exfat` | `keel_vfs::fuzz::recover_exfat` | deleted-file recovery on an exFAT volume image: directory entry sets and their checksums, FAT chains, the allocation bitmap, then each file's data |
| `recover_fat` | `keel_vfs::fuzz::recover_fat` | deleted-file recovery on a FAT12/16/32 volume image: the BPB, the FAT, folders and long names, then each file's data |
| `recycle_bin` | `keel_vfs::fuzz::recycle_bin` | a Windows Recycle Bin `$I` file (versions 1 and 2) |
| `tiff` | `keel_core::fuzz::tiff` | a TIFF's orientation (first IFD) and metadata |
| `raw` | `keel_core::fuzz::raw` | camera RAW files: the walk of IFDs and SubIFDs to the embedded JPEG previews (bounded in count and depth), metadata, small previews |
| `image_meta` | `keel_core::fuzz::image_meta` | photo metadata: EXIF, XMP, dimensions, small thumbnails |
| `probe` | `keel_core::fuzz::probe` | ffprobe JSON as the video metadata reader takes it |
| `query` | `keel_core::fuzz::query` | the library search box (`LibraryQuery::parse`, media filters, `content:` terms) and the SQL it becomes; every FTS5 MATCH string it builds (names and the text inside documents) must run without an error |
| `efu` | `keel_search::fuzz::efu` | file lists: the first byte picks EFU, CSV, TSV, TXT or M3U, the rest is the file, read under small bounds (10,000 rows, 4 KiB lines); what is read is written in every export format, the EFU written back must read back with as many entries and the same columns, and the list is searched |
| `backup` | `keel_core::fuzz::backup` | a library backup (`.keelbackup`): the zip, the manifest and its bounds (`inspect`), then a restore into a scratch data folder (hashes, database checks) |
| `finder_tags` | `keel_core::fuzz::finder_tags` | the macOS Finder tag attribute (a binary property list of tag strings): read, written again and read back the same |
| `link` | `keel_core::fuzz::link` | `keel:` links (the URL protocol, from web pages and other apps): a parsed link only searches or shows a local absolute folder (no network or device path, no `..`), and its own URL reads back the same |
| `rpc` | `keel_api::fuzz::rpc` | JSON-RPC lines from the daemon socket, each request's params decoded by its operation |
| `mcp` | `keel_api::fuzz::mcp` | an MCP stdio session, with and without `--allow-execute` |
| `wire` | `keel_net::fuzz::wire` | keel-net messages (CBOR): requests, Spacedrop offers, sync pages (applied to a library), pairing tickets |
| `pair_code` | `keel_net::fuzz::pair_code` | pairing codes and tickets as typed or scanned, device ids |
| `multipart` | `keel_daemon::fuzz::multipart` | a Web Share Target upload: `Content-Type` line, then the `multipart/form-data` body |
| `everything_query` | `keel_daemon::fuzz::everything_query` | an Everything HTTP query string for `--web`'s `/everything`: parsed, written back as a link and parsed again, its search compiled and checked against a few names, the HTML and JSON answers made; the same bytes as an `Authorization` header |
| `etp_command` | `keel_daemon::fuzz::etp_command` | ETP command lines for `--etp`: each parsed as the server parses it, `SITE EVERYTHING` settings applied, paths resolved from the current folder (never above `/`), a `QUERY` reply written and read back by the ETP client; the same lines as a `QUERY` reply for the client, the first as a `-connect` target |
| `preview` | `keel_preview::fuzz::preview` | the preview panel: the first byte picks the type (`keel_preview::fuzz::EXTENSIONS`), the rest is the file (text in any encoding, CSV, xlsx/xls/ods, docx, pptx, OpenDocument, SVG, images), then the same file through `extract_text`, the library's text reader |
| `font` | `keel_preview::fuzz::font` | font files (TrueType, OpenType, collections): the name table and the sample drawn in the font, with glyph boxes checked before they are rasterized |
| `query_parse` | `keel_query::fuzz::query_parse` | the search language (`keel_query::parse`): any text with three option sets (OR or AND precedence, literal operators, slashes, regex, whole words), then every query that compiles evaluated against a few items with edge dates and sizes; groups and macros are depth-limited and regexes size-limited |
| `file_signature` | `keel_props::fuzz::file_signature` | file signatures (magic numbers, zip and PE heads), the text check, text statistics (encoding, lines, words) and `.url` files |
| `lnk` | `keel_props::fuzz::lnk` | Windows shell links (`.lnk`): header, IDList, LinkInfo (local path, network share, suffix), string data, extra data blocks |
| `pe_version` | `keel_props::fuzz::pe_version` | PE images: headers, sections, the resource directory walk to `RT_VERSION` (bounded in sections and entries) and the `VS_VERSIONINFO` blocks |
| `audio_tags` | `keel_props::fuzz::audio_tags` | audio files: ID3v2 (2.2 to 2.4, unsynchronisation, extended headers), ID3v1, MPEG frames with Xing/VBRI, FLAC metadata blocks and Vorbis comments, WAV chunks |
| `content_match` | `keel_props::fuzz::content_match` | content search: the first byte picks the encoding (`content:`, `utf8content:`, `utf16content:`, `utf16becontent:`, `ansicontent:`, `binary:`, `hex:`) and options (case, whole words, regex, line mode), the bytes up to a NUL are the pattern, the rest is the file, searched streamed, as library text and under a byte cap |
| `recycle_info` | `keel_props::fuzz::recycle_info` | Windows Recycle Bin `$I` records (versions 1 and 2) |
| `trash_info` | `keel_props::fuzz::trash_info` | freedesktop.org `.trashinfo` files (group, percent-encoded path, deletion date with or without a zone) |
| `mft_record` | `keel_search::fuzz::mft_record` | NTFS MFT `FILE` records as the index service reads them from a volume: the update sequence, every attribute (standard information, file names, data streams, reparse point, attribute list), data runs, and records cut into 1 KiB pieces merged into files (extension records, hard links); each piece also blanked as the elevated helper does before sending it (the parse must not change), framed and decoded again as Keel receives it, and the whole input decoded as a helper frame |
| `index_proto` | `keel_search::fuzz::index_proto` | the index service's pipe protocol as both ends read it: length-prefixed frames (bounded in size), every request and response (drive letters, counts checked against the bytes left, file names that must be one path component), and each message that decodes encoded again to the same message |

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
