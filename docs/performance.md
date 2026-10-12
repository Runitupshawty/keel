# Performance

How fast Keel is on realistic amounts of data, what grows with what, and how to measure it on your own machine. Every number below comes from a release-mode test that is `#[ignore]`d in the normal test run and asserts a generous budget, so a nightly job can run them all with `scripts/perf.sh` (see [Running the measurements](#running-the-measurements)).

## Numbers

One desktop PC, release build, 2026-10-10 (0.15.0 plus the changes listed under [Unreleased] in the changelog): AMD Ryzen 7 3700X (8 cores), 32 GB RAM, NVMe SSD, Windows 11 with its real-time antivirus scan on. "Warm" means the files were in the file cache (as right after they were written); "cold" means they were not and had to come from the disk, through the antivirus scan.

### Library

| Measurement | Data | Result | Budget |
| --- | --- | --- | --- |
| Index a folder source (`perf_index_200k_files`) | 200,000 files in 20,200 folders on disk | first walk 9.1 s warm (24,300 records/s), 17.3 s cold; unchanged re-walk 1.7-1.9 s; `source.db` 115 MB, `library.db` 0.15 MB; peak memory 94 MB | 60 s |
| The same tree, generated (`perf_index_small_folders_generated`) | 220,201 records, no disk | 5.1 s (42,800 records/s) | 9 s |
| Folder totals (`perf_folder_totals`, 2026-10-11, the machine busy with other builds), first walk without the totals triggers / with them and the fill at the end | 220,201 generated records in 20,201 folders; 2,002,001 in 2,001 | 4.7 s / 5.0 s (+5 %), the fill alone 0.05 s; 58.4 s / 51.4 s (within the noise of a busy machine), the fill alone 0.62 s | under 10 % (asserted at 25 %) |
| Index a generated tree (`two_million_entries_index_fast_in_bounded_memory`) | 2,002,001 records, 2,000 folders of 1,000 | 41-51 s; unchanged re-walk 7.8-10.7 s; peak memory 153 MB; FTS queries 0.2-1.2 ms | 90 s, 400 MB, 50 ms |
| Index and search realistic paths (`realistic_paths_search_is_fast`) | 2,000,000 files five folders deep | index 57 s; 12 ranked queries 1.1-26.7 ms | 50 ms a query |
| Watcher burst (`perf_watcher_burst_10k`) | 10,000 new files in a watched source | all indexed 3.5 s after the first write (1.1 s of it writing the files, 0.5 s the debounce) | 5 s |
| Hash small files (`ten_thousand_small_files_hash_fast`) | 10,000 files of a few bytes | 1.1-1.4 s | 5 s |
| Hash 1 MiB files (`perf_hash_10k_files_of_1mib`) | 10,000 files of 1 MiB, 1,000 of them copies (hashed whole) | 2.6-2.8 s warm (3,600 files/s); 44-83 s cold (reading the 10 GiB once at normal priority took 31 s) | 20 s (warm) |
| `library.search` (`perf_search_200k`), p50 / p95 of 50 runs | 200,000 records, 2,000 tagged `work` | `w4242` 0.13 / 0.13 ms; `invoice budget` 5.9 / 6.2 ms; `contract w42` 1.7 / 1.7 ms; `tag:work` 1.3 / 1.3 ms; `tag:work invoice` 6.3 / 6.4 ms; `tag:work ext:pdf` 2.2 / 2.4 ms | p95 25 ms |
| `library.search` (`two_million_row_search_is_fast`) | 2,002,001 records, 13 queries (words, phrases, prefixes, filters only) | 0.2-25.9 ms | 50 ms |
| Duplicates (`perf_duplicates_and_recount_200k`) | 200,000 hashed records, 50,000 contents | 0.38 s | 5 s |
| Protection recount (same test; `recount_100k`) | 200,000 records; 100,000 records in two sources | 0.27 s; 0.11 s (library stats 2.3 ms) | 5 s |
| Sidecar job (`perf_sidecar_5k_jpegs`) | 5,000 JPEG photos of 1024 x 768 | 48-54 s warm (92-104 photos/s); reading them once cold took 15 s | 120 s |
| Sidecar job (`sidecar_perf_1000_images`) | 1,000 PNGs of 64 x 64 | 6.1-8.8 s | 10 s |
| Text job (live check, 2026-10-11, release build through keel-daemon) | 200 generated documents of 11 kinds (text, Markdown, code, CSV, docx, xlsx, pptx, odt, ods, odp, PDF), about 570 characters each | all 200 read in 1.1 s after the walk ended (about 180 documents and 100,000 characters a second); content searches 29 hits with snippets each | under a minute |
| Copy through a plan (`three_thousand_items_copy_fast`, 2026-10-11) | 3,000 one-byte files | 2.9-3.1 s through a plan (about 1,000 files/s); the plain copy 0.87-1.02 s (2,950-3,450 files/s), 1.4-1.7 s with `KEEL_COPY_THREADS=1`. 0.19.0: 3.6-3.9 s and 3.7-3.8 s | 2.66 s over the plain copy; the plain copy 2.5 s |

### Folders, archives and the window

| Measurement | Data | Result | Budget |
| --- | --- | --- | --- |
| List a local folder (`perf_list_50k`) | 50,000 files | 61 ms | 150 ms |
| Open a folder in the window (`perf_open_100k_folder`) | 100,000 files, both panes | listed, sorted and shown in 0.24 s warm, 1.2 s cold; slowest frame after 0.9 ms | 5 s, 50 ms |
| Filter and refresh a tab (`perf_100k`) | 100,000 entries | refresh 1.0 ms; a keystroke 0.7 ms (typing) / 1.0 ms (Backspace); lowercasing the names (worker) 56 ms | 16 ms |
| Scroll the media grid (`media_grid_perf`) | 129,000 items with sidecars, M and L tiles at 1.5x and 2x | mean frame 0.5-1.9 ms, p95 1.2-5.4 ms, worst 8.2 ms; textures at most 379 MiB | mean 16 ms, every frame 50 ms |
| List a zip (`perf_zip_list_10k_under_200ms`) | 10,000 entries | 58 ms | 200 ms |
| Extract a zip (`perf_zip_extract_3k_under_2s`, `perf_zip_extract_10k`) | 3,000 / 10,000 entries | 1.1 s / 4.7 s (2,150 entries/s) | 2 s / 20 s |
| Delete one entry of a zip (`perf_zip_delete_one_entry_of_1gib`) | 1 GiB, 1,024 entries | 1.4 s (one rewrite, 774 MB/s) | 20 s |
| Search an NTFS index (`perf_two_million_entries`, keel-search) | 2,000,000 generated entries | built in 0.49 s; queries 0.9-31 ms | |
| Read an NTFS volume's file table (`live_raw_mft_matches_enumeration`, keel-search, 2026-10-12, elevated, the machine busy with other builds) | 1,534,000 / 1,735,000 / 169,000 files on three NTFS volumes | raw MFT read with every metadata column 4.6 s / 14.6 s / 2.8 s; `FSCTL_ENUM_USN_DATA` (names and attributes only) 5.4 s / 41.8 s / 2.0 s; sizes equal to the file system's for all 15,000 files sampled | |
| Folder totals and fast sort (`perf_two_million_entries`, keel-search, 2026-10-12) | 2,000,000 generated entries with sizes and dates | totals recounted in 0.16 s; fast-sort orders by name, size and modified built in 1.4 s; 1,000 changes applied in 2 ms and settled (totals and the three orders) in 0.34 s; sorting 200,000 results by size 7.8 ms through the order, 27 ms without | |
| Search a walk index (`bench_synthetic_tree`, keel-search) | 100,000 files on disk | indexed in 0.49 s; queries 0.3-5.6 ms | |

### Devices, daemon, web client, startup

| Measurement | Data | Result | Budget |
| --- | --- | --- | --- |
| Spacedrop (`two_thousand_files_arrive_in_linear_time`) | 2,000 empty files between two in-process nodes | 2.4 s (the first 1,000 in 1.2 s: linear) | 3 s or 3x the bare requests, plus one fsync a file |
| Daemon `list` over the local socket (`perf_list_1000_requests`) | 1,000 requests, 102 entries each, one client | 0.64 s (1,565 requests/s) | 10 s |
| Web client bundle (`scripts/build-web.sh`) | `crates/keel-web/dist` | 3.48 MB in all; the wasm 3.37 MB, 1.50 MB gzipped | 6 MB of wasm |
| `keel --version` (`perf_version_startup`) | process start to exit, median of 20 | 78 ms (slowest 316 ms) | 500 ms |
| Window to first frame (`perf_first_frame`, kittest with wgpu) | the app built, one frame run and rendered | 0.62 s (0.59 s of it creating the app and the GPU device); both panes listed by then | 3 s |

## What scales how

- **Indexing** is linear in the number of entries, and memory stays flat (rows stream in transactions of up to 5,000): 94 MB at 200,000 entries, 153 MB at 2,000,000. Most of an entry's cost is SQLite: the row, its identity indexes and the full-text index with 2- and 3-character prefixes. Listing a folder on Windows costs about 0.2 ms besides its entries, so a tree of many small folders indexes more slowly per entry than one of big folders. A walk that finds nothing changed is 5 to 10 times faster than the first.
- **Search** depends on how many records match, not on the library's size: a rare word answers in well under a millisecond in 2,000,000 records. A common word costs about 1 us a match up to 50,000 matches (ranked by bm25); above that the first matches by record id are taken, so the cost stops growing. A `tag:` query with words starts from the tag's records when the tag holds at most 20,000.
- **Duplicates and the protection recount** copy every hashed record into a scratch database and group them: about 2 us a record, so about 4 s at 2,000,000.
- **Hashing** reads files up to 192 KiB whole and bigger ones as three 64 KiB samples (whole only when samples collide), so its cost is opening files, not their size. Hashing and the sidecar job read at background I/O priority: from a cold cache, with the disk and the antivirus scan to wait for, they run several times slower than the warm numbers above, on purpose (they must not slow down what you are doing).
- **The text job** reads one document at a time on a helper thread at idle priority, so its rate is set by the readers: small text files and Office documents take about 5 ms each, a PDF its pages' text through pdfium. Each document is capped (8 MiB in, 64 MiB for a PDF; 200,000 characters kept; 10 s), so one large file cannot hold it up for long, and it pauses 5 s after your last input. Its search index (`record_content`) keeps the text once, in the source's store, so the store grows by about the size of the text read plus its index. There is no cap on a store as a whole, only per document: at the 200,000-character cap a document keeps 0.2 MB of English text (one byte a character) to 0.6 MB of Chinese (three), and its index about as much again, so 10,000 documents that all reach the cap would add about 3 to 4 GB to their source's store. Typical documents are far smaller (the live check's averaged 570 characters, about 1 KB each with the index).
- **The watcher** waits until changes stop for 0.5 s (at most 5 s into a continuous burst), then applies about 1,000 paths in 0.2 s.
- **The sidecar job** spends about 10 ms on a 1024 x 768 JPEG on one thread: decoding 1.5 ms, scaling down 2.4 ms, WebP 0.3 ms, the first read (antivirus scan) about 3 ms, the rest writing the sidecar files.
- **Copying files**: every file is copied to a staging name (`<name>.keel-partial-<pid>-<n>`) and renamed into place, so a cancel or an error never leaves half a file. Files up to 1 MiB are copied to their staging names by a pool of worker threads, min(8, the processor's threads) of them, while the transfer's own thread walks the folders, makes each folder before anything goes into it, decides each conflict and places the finished copies in the order it queued them: what lands, the progress, the resume record of a library operation and the point where an error stops are those of a copy of one file at a time, and the first error or a cancel leaves the copies still queued unplaced (their staging files are deleted). The walk runs ahead of the pool, though: folders it makes, and in a move the files and folders it renames on the same drive, can be done before the error of a file queued earlier is seen; the job still stops at that file. The pool starts only for two files or more. A staging file is swept as left over only when a day old and made by another process, never one this process may still be writing. Bigger files are copied one at a time, each in one stream that reports its progress as it goes, once the queued ones are placed. `KEEL_COPY_THREADS` sets the number of workers (1 to 64; 1 copies one file at a time), for measurements. On this machine one small file costs about 300 us to copy and 120 us to rename into place, through the antivirus scan: the copies run in parallel (eight workers copy 3,000 files in about 0.4 s), the renames do not (about 0.37 s for 3,000 however many threads rename), which is what limits a plain copy of small files to about 3,000-3,500 a second. Atlas's benchmark, 2,000 files of 4 KiB, takes 0.62-0.74 s here (0.94 s one at a time). Workers also copy files of 1 MiB 2.2 times and of 4 MiB 1.8 times faster than one at a time, but those are copied with progress per chunk instead.
- **A copy through a plan** (the library's preview, then a job) adds its own work to every batch of 64 top-level items: checking each item is still there, the index update for what the batch made, and the job's record of it in the library database (an fsync). Of 2.9 s for 3,000 one-byte files, the copying is about 1.0 s, the index update 0.7 s, the job records 0.6 s and the preview 0.12 s (it is checked again when the job starts).
- **Zip extraction** costs about 0.45 ms an entry on this machine, nearly all of it creating the file (each entry is written to a temporary file and renamed into place, so a cancel never leaves half a file). **Zip edits** rewrite the archive once, copying the other entries byte for byte at disk speed.
- **Folders in the window**: listing costs about 1.2 us an entry, and a frame costs the same with 100 or 100,000 entries (only the visible rows are laid out).

## Running the measurements

They need a release build (in a debug build several budgets do not hold) and about 15 GB of free space for fixtures, which are made once and kept under `target/` (`perf-hash-10k` is 10 GiB, `keel-media-perf` about 3 GB, `perf-zip-1g.zip` 1 GiB, the rest are many small files). Run them all, or those whose name contains a word, from the repository root:

```sh
scripts/perf.sh            # everything, then a table
scripts/perf.sh search     # only measurements whose name contains "search"
KEEL_PERF_NO_GPU=1 scripts/perf.sh   # skip the two that need a GPU
```

The script points `KEEL_CONFIG_DIR` and `KEEL_DATA_DIR` at a temporary folder (never your own settings or library), runs one test at a time with `--test-threads 1`, and prints a table with each measurement's result, wall time and output. A row that says FAILED went over its budget. Build the web client first (`scripts/build-web.sh`) to include its size. Close other heavy programs: the budgets are generous, but the numbers are only comparable on an idle machine.

One measurement on its own, for example:

```sh
cargo test --release -p keel-core --lib perf_search_200k -- --ignored --nocapture
cargo test --release -p keel-app --bin keel perf_open_100k_folder -- --ignored --nocapture
cargo test --release -p keel-vfs --test zip_edit perf_ -- --ignored --nocapture
```

## Not fixed yet

- Hashing and the sidecar job from a cold cache (see above): by design, they stay at background I/O priority.
- The first walk of a tree of small folders still lists one folder after the other; listing the next folders on another thread while the rows are written would save up to the listing time (about 4 s of the 9 s for 200,000 files in 20,200 folders here).
- The sidecar job makes one photo at a time, and decodes the whole image before scaling it down. Decoding at a reduced size, or a few photos at once while the machine is idle, would make it several times faster.
- Zip extraction is bound by creating one temporary file and one rename per entry.
- A copy through a plan spends about two thirds of its time on its own bookkeeping (see above), so it copies about 1,000 small files a second where a plain copy does 3,000: indexing what a batch made and recording the batch in fewer, larger writes would close most of that gap.
- The web client's wasm is built for speed (`opt-level = 3`), not size; `wasm-opt -Oz` or `opt-level = "z"` for the wasm build would make it smaller.
- On Windows a burst of changes bigger than the watcher's buffer can lose events without notice (the full re-walk every 6 hours finds them). 10,000 new files did not, here.
