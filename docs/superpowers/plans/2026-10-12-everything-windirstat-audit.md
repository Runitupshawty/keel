# Everything and WinDirStat: feature checklist and Keel audit

Date: 2026-10-12. Keel: 0.22.0 (`main` f2b0c9b). This file is the evidence base for Phases 15 to 17 ([15](2026-10-12-keel-phase15.md): the search engine, [16](2026-10-12-keel-phase16.md): search UI, file lists, servers and automation, [17](2026-10-12-keel-phase17.md): disk usage). Part 1 is every feature of voidtools Everything (1.4.1 and the 1.5 alpha) and WinDirStat (1.1.2 and 2.x up to 2.9.3), checked against their own pages. Parts 2 to 4 say, for each, whether Keel has it, with the file and line. Paths are relative to the repository root. "E passthrough" means the feature works in a pane search tab only because the query is handed to a running Everything unchanged.

---

# Part 1: verified feature lists

URL keys: S=https://www.voidtools.com/support/everything/searching/ ; F=.../everything/search_functions/ ; M=.../everything/search_modifiers/ ; C=.../everything/command_line_options/ ; E=.../everything/command_line_interface/ ; O=.../everything/options/ ; H=.../everything/http/ ; T=.../everything/etp/ ; L=.../everything/file_lists/ ; K=.../everything/sdk/ ; A=https://www.voidtools.com/forum/viewtopic.php?t=9787 (1.5 alpha changelog; only first ~100k of 148k chars read) ; GR=https://github.com/windirstat/windirstat ; GC=https://raw.githubusercontent.com/windirstat/windirstat/master/CHANGELOG.md ; GRM=https://raw.githubusercontent.com/windirstat/windirstat/master/README.md ; WN=https://windirstat.net/
(Tag shorthand: [confirmed: S] means confirmed at the page named by that key.)

## 1. voidtools Everything (1.4.1 stable + 1.5 alpha)

### Indexing
- NTFS MFT read + USN journal live updates [confirmed: O (NTFS tab: Enable USN Journal, Maximum size, Allocation delta, Load recent changes from USN journal, Monitor changes)]
- ReFS volumes with USN journal options [confirmed: O (ReFS tab)]
- FAT/exFAT volumes with change monitoring attempt, buffer size, rescan on full buffer, Update/rescan [confirmed: O (FAT tab)]
- Network drives index (monitor attempt, buffer size, rescan on full buffer, fast rescan) [confirmed: O (Network Drives tab)]
- Folder indexes (folders list, monitor, buffer size, fast rescan, Update) [confirmed: O (Folders tab)]
- File lists added to index, auto-reload unless "Monitor changes" off [confirmed: L, O]
- Network Indexes: remote Everything indexes with password, monitor changes [confirmed: O (Network Indexes tab)]
- Automatically include new fixed / removable volumes; automatically remove offline volumes; Include in database / Include only [confirmed: O]
- Indexed properties: file size, folder size, date created, date modified, date accessed, attributes; each with "fast sort" option; fast extension sort [confirmed: O (Indexes tab)]
- Index recent changes, Enable journal (with Maximum size), Force Rebuild, database location, multi-user database filename [confirmed: O (Indexes tab)]
- Properties tab (1.5): index any property; Include/Exclude folders/files, Include only folders/files, Exclude not content indexed, Exclude recall on data access, Maximum size, Fast sort [confirmed: O (Properties tab)]
- Exclude tab: exclude hidden, exclude system, exclude folders, include only files, exclude files (wildcard) [confirmed: O (Exclude tab)]
- Everything service (run non-admin), "Run indexing process as administrator", "Run as administrator" [confirmed: O (General), C (-install-service, -index-as-admin, -admin)]
- Multiple instances (-instance) [confirmed: C]
- Portable / config file: -config <ini>, "Store settings and data in %APPDATA%\Everything" toggle [confirmed: C, O]
- Database: -db, -read-only, -no-db, -load-delay, -save-db, -reindex, -monitor-pause/resume/start/stop [confirmed: C]
- Safe mode (plugins and icon shell extensions disabled), -no-policy, group policy / policy.ini / user.ini (1.5) [confirmed: C, A]
- Create/delete USN journal from command line (-create-usn-journal, -delete-usn-journal) [confirmed: C]
- .metadata.efu sidecar files supply extra properties (1.5) [confirmed: L, A]
- Offline volumes handled/performance improved (1.5) [confirmed: A]
- Plugins tab: enable plug-ins, enable individual plug-in [confirmed: O]
- Advanced settings tab (show only modified settings, search settings, value editor) [confirmed: O]
- Import / Export / Reset settings wizard [confirmed: O]
- Content indexing (1.5; ansi/utf8/utf16 content properties) [confirmed: A]
- Sidecar hash / folder hash / digital signature properties (1.5) [confirmed: A]

### Search syntax
- Space = AND, `|` = OR, `!` = NOT, `<>` grouping, `"quotes"`, `*` and `?` wildcards, `\` path search, trailing `\` folder, `..` ranges, `;` list separator [confirmed: S (? wildcard not named on S, but standard; see F)]
- Regex syntax (alternation, groups, classes, anchors, quantifiers {x}, {x,}, {x,y}) [confirmed: S]
- Search option toggles: Match Case, Match Diacritics, Match Whole Words, Match Path, Match Prefix, Match Suffix, Ignore Punctuation, Ignore White-space, Regular Expressions [confirmed: S, O (Home tab), C]
- Numeric comparison forms: `<= < = == > >= != !`, `start..end`, `start-end`, `unknown` [confirmed: F]
- Search-time options in Options > Search: search as you type, search delay, select search on mouse click, focus search on activate, match whole filename when using wildcards, allow literal operators, replace forward slashes with backslashes, operator precedence [confirmed: O]
- Operator precedence modifiers: and-precedence:, or-precedence: [confirmed: M]
- Substitution variables `$property:` and `$0:`..`$9:` regex captures [confirmed: F]
- Property alias tags in the form [search-modifier:]function:value; `function*:` for literal text [confirmed: F]

### Search modifiers (complete list from the modifiers page)
- all-strings: [confirmed: M]
- and-filter: [confirmed: M]
- and-precedence: [confirmed: M]
- binary: [confirmed: M]
- case: / no-case: [confirmed: M]
- diacritics: / no-diacritics: (alias diac:) [confirmed: M]
- dot-all: [confirmed: M]
- each-line: [confirmed: M]
- end-with: (alias $:) [confirmed: M]
- no-fast-regex: [confirmed: M]
- file: [confirmed: M]
- folder: [confirmed: M]
- global: [confirmed: M]
- hex: [confirmed: M]
- hex-to-number: [confirmed: M]
- no-highlight: [confirmed: M]
- from-disk: [confirmed: M]
- len: [confirmed: M]
- menu-break: [confirmed: M]
- menu-separator: [confirmed: M]
- metric: / no-metric: [confirmed: M]
- multi-line: [confirmed: M]
- multi-string: / no-multi-string: [confirmed: M]
- no-!: [confirmed: M]
- or-filter: [confirmed: M]
- or-precedence: [confirmed: M]
- path: / no-path: (alias \:) [confirmed: M]
- pinyin: / no-pinyin: (py:), pinyin-jian: (pyj:), pinyin-quan: (pyq:) [confirmed: M]
- prefix: / no-prefix: [confirmed: M]
- ignore-punctuation: / no-ignore-punctuation: (nopunc:) [confirmed: M]
- regex: [confirmed: M]
- enable-result-omissions: / disable-result-omissions: [confirmed: M]
- no-reorder: [confirmed: M]
- search-current-results: / no-search-current-results: [confirmed: M]
- soundex: [confirmed: M]
- star-all: [confirmed: M]
- start-with: (alias ^:) [confirmed: M]
- no-substitute: [confirmed: M]
- suffix: / no-suffix: [confirmed: M]
- enable-temp-result-omissions: / disable-temp-result-omissions: [confirmed: M]
- to-integer:, to-number: (#:), to-string: [confirmed: M]
- ungreedy: [confirmed: M]
- whole: (aliases whole-filename:, wfn:, entire:) [confirmed: M]
- ignore-whitespace: / no-ignore-whitespace: (nows:) [confirmed: M]
- whole-words: / no-whole-words: (ww:) [confirmed: M]
- wildcards: / no-wildcards: [confirmed: M]

### Search functions (complete list from the search functions page; aliases in parentheses)
Name/path/type:
- name:, stem:, extension:, ext:, full-path:, full-path-length: (fullpathlen:), name-length: (namelen:), parent:, parent-name:, parent-count: (depth:), parent-file-id: (parentfrn:), root:, type:, kind:, perceived-type:, content-type:, document-content-type:, file-signature:, shortcut-target:, volume-label:, location: (loc:), computer:, url: [confirmed: F]
Size/count:
- size:, size-on-disk:, count:, child:, child-file:, child-folder:, child-count:, child-file-count:, child-folder-count:, descendant:, descendant-file:, descendant-folder:, descendant-count:, descendant-file-count:, descendant-folder-count:, sibling:, sibling-file:, sibling-folder:, ancestor:, ancestor-name:, empty:, exists: (file-exists:, folder-exists:), hard-link-count:, hard-link-filenames:, alternate-data-stream-count: (adscount:), alternate-data-stream-names: (adsnames:), container-file-count:, container-filenames: [confirmed: F]
Dates:
- date-modified: (dm:), date-created: (dc:), date-accessed: (da:), date-recently-changed: (rc:), date-run:, date-deleted:, date-saved:, date-printed:, date-received:, date-sent:, date-released:, date-acquired:, date-changed:, date-content-created:, date-taken:, date-encoded: (date-media-created:), plus component forms for dm/dc/da: -date, -day, -day-of-week, -hour, -month, -time, -year (e.g. dmdate:, dmday:, dmdow:, dmhour:, dmmonth:, dmtime:, dmyear:; same for dc and da) [confirmed: F]
Attributes/misc file:
- attributes: (attr:, attrib:), owner:, protected:, offline:, online:, offline-availability:, offline-status:, run-count:, is-running:, file-id: (frn:), filelist:, filelist-filename:, filelist-full-path:, network-index-host:, shared-with:, status:, content-status:, character-encoding:, line-count:, word-count:, page-count:, slide-count:, total-editing-time:, md5:, sha1:, sha256:, crc32:, crc64: (crc:), digital-signature-name:, digital-signature-timestamp:, version:, product-name:, product-version:, company:, copyright:, trademarks:, description:, details:, system-index: (si:), content: [confirmed: F]
Duplicates/uniqueness:
- dupe:, dupe-count:, dupe-from:, dupe-max:, dupe-min:, distinct:, distinct-sort:, unique: [confirmed: F]
Media: image
- width: (wide:), height: (high:), dimensions:, orientation:, bit-depth: (bpp:), horizontal-resolution:, vertical-resolution:, aspect-ratio:, camera-maker:, camera-model:, lens-model:, aperture:, f-stop:, max-aperture:, exposure-time:, exposure-bias:, exposure-program:, exif-version:, flash-mode:, focal-length:, 35mm-focal-length:, iso-speed:, light-source:, metering-mode:, brightness:, contrast:, saturation:, white-balance:, subject-distance:, latitude:, longitude:, altitude: [confirmed: F]
Media: audio/video/music
- album:, album-artist:, artist:, authors:, composer:, conductor:, director:, producer:, writer:, genre:, mood:, title:, subtitle:, track:, year:, beats-per-minute: (bpm:), initial-key:, length: (duration:), audio-bitrate:, audio-bits-per-sample:, audio-channels:, audio-format:, audio-sample-rate:, audio-track-count:, video-bitrate:, video-format:, video-track-count:, subtitle-track-count:, total-bitrate:, frame-count:, frame-rate: (fps:), language:, part-of-a-compilation:, part-of-set:, period:, encoded-by:, publisher:, parental-rating:, parental-rating-reason: [confirmed: F]
Document/other:
- subject:, comment:, category:, tags: (tag:), rating:, people:, last-author:, software:, from:, to:, preview:, thumbnails: [confirmed: F]
UI-state functions (usable in bookmarks/filters/macros):
- columns:, add-columns: (addcol:), remove-columns:, layout:, add-layout:, remove-layout:, sort:, sort-ascending:, sort-descending:, sort-mix: / no-sort-mix:, folders-first: / no-folders-first:, folders-first-ascending:, folders-first-descending:, alternate-row-color: / no-alternate-row-color:, on-top: / no-on-top:, folders-sidebar:, bookmarks-sidebar:, filters-sidebar: (each with no- form), preview: / no-preview:, max-threads:, regular-expression-match0..9: (regmatch0..9:) [confirmed: F]
- Filter macros: audio:, doc:, exe:, image:, video:, zip: [confirmed: S]
- Date constants, size constants (empty/tiny/small/medium/large/huge/gigantic), recently-changed [unverified: pages "Date constants"/"Size syntax" not fetched]
- Search functions in the alpha changelog: parent-sibling, ancestor-sibling, pathlist, complete-word (Ctrl+Space), and-filter, multistring, textplain, parentsize/parentdm, ISO-8601 wildcard dates, synonym lists; formula functions ISERR ISEVEN ISODD ISLOGICAL ISNONTEXT ISNUMBER ISTEXT; REGEXEXTRACT [confirmed: A]

### UI / options areas
- General tab: Language, store settings in %APPDATA%, check for updates, start on system startup, Everything service, run indexing/Everything as admin, folder context menu item, Start menu shortcuts, desktop shortcut, URL protocol, EFU file association [confirmed: O]
- UI tab: Scale, run in background, show tray icon, single-click tray, create new window from tray / when running Everything, open searches in tabs, switch to new tabs immediately [confirmed: O]
- Home tab: default match toggles plus Search, Filter, Columns, Sort, View, Index, File list, ETP server, Link type [confirmed: O]
- Results tab: hide results when search empty, clear selection on search, close window on execute, open path with double click in path column, auto scroll, double-quote copy as path, do not select extension when renaming, sort date/size descending first, full row select, single click open, underline icon titles [confirmed: O]
- View tab: alternate row color, row mouseover, highlighted search terms, selected item/total size in status bar, size standard, size format [confirmed: O]
- Context Menu tab: per-item visibility (show / show with Shift / hide / default) and command [confirmed: O]
- Fonts and Colors tab: theme, item, font, size, accent color, item states, foreground/background/font style [confirmed: O]
- Keyboard tab: New window, Show window, Toggle window hotkeys; configurable command shortcuts (Add/Edit/Remove) [confirmed: O]
- History tab: search history, run history, always show suggestions, sort order [confirmed: O]
- Dark mode / themes (1.5) [confirmed: O (Fonts and Colors > Theme); dark mode wording itself [unverified]]
- Tabs, sessions (restore previous session), folders sidebar, filters sidebar, bookmarks sidebar, filter bar, preview pane/sidebar, status bar, thumbnails view and sizes, details view, fullscreen, always-on-top, zoom/scale [confirmed: C, A]
- Column labels, column colors, column formulas, column context-menu customization (1.5) [confirmed: A]
- Result omissions and temporary result omissions [confirmed: C, M]
- Run count / date run tracking; select most-run / last-run result [confirmed: C]
- Index Journal (1.5, view change history) [confirmed: A, E]
- Custom open commands, custom filters, bookmarks (-bookmark, -filter) [confirmed: A, C]
- Localization (many languages) [confirmed: A, C (-language)]
- Advanced Rename (copy/move/rename with file list input) [confirmed: C]
- Custom -command <id> UI command invocation [confirmed: C]

### File lists
- .efu = UTF-8 CSV with header, Filename required, optional Size, Date Modified, Date Created, Attributes, and other properties; FILETIME or ISO-8601 dates [confirmed: L]
- Open .efu, .txt, .csv, and .db / .backup.db (db needs 1.5) [confirmed: L]
- File > Export: EFU (1.5), selection only; editor exports CSV, EFU, M3U, M3U8, TXT (tab-delimited), TXT [confirmed: L]
- File List Editor: drag-drop add, Import/merge, Remove Duplicates, Update rescan, new list [confirmed: L]
- Everything.exe -create-file-list with include/exclude folder/file filters, no-subfolders, no-folders, no-files [confirmed: C, L]

### Remote
- ETP/FTP server: SITE EVERYTHING extension, 4 link types, SUBST sharing, bind interface, port, username/password (plain text, no TLS), logging, allow download, service mode, separate instance, welcome message via Plugins.ini [confirmed: T]
- ETP client: Connect to ETP server, Home tab auto-connect, -connect [user[:pass]@]host[:port] [confirmed: T, C]
- HTTP server: search/browse in browser, Basic auth (no TLS), bind, port (default 80), logging, serve custom pages, default page, allow download, service mode, separate instance [confirmed: H]
- HTTP URL params: search/s/q, offset/o, count/c, json/j, case/i, diacritics/m, wholeword/w, path/p, regex/r, prefix, suffix, ignore_punctuation, ignore_whitespace (last four 1.5), sort (name/path/date_modified/size), ascending, path_column, size_column, date_modified_column [confirmed: H]
- Everything Server (1.5 alpha plugin, no result window limit, client-server) and Everything Server/ETP/HTTP as plugins in 1.5 [confirmed: A (ETP/FTP, HTTP Server, Everything Server plugins listed); T (recommends Everything Server)]
- Network index sharing (Network Indexes tab) [confirmed: O]

### Automation
- Command line (Everything.exe): full switch set across Searching, Results, Window, General, Database, File Lists, Advanced Rename, ETP, Installation (see C) [confirmed: C]; notable: -search/-s, -path/-p, -parent-path, -parent, -name-part, -stem, -extension, -filter, -bookmark, -home, -explore, -url, -sort variants incl. secondary/tertiary, -details, -thumbnails, -columns, -select-*-result, -new-window, -new-tab, -toggle-window, -close, -startup, -restore-session, -exit, -restart, -debug, -svc
- es.exe: export formats -csv -efu -json -m3u -m3u8 -tsv -txt and -export-*; -no-header, -utf8-bom, -sort, DIR-style /o flags, /ad /a-d /a<attrs>, column toggles, -add-columns, -set-column-colors/widths, -size-format, -date-format, -highlight, -count, -get-result-count, -get-total-size, -get-folder-size, run-count get/set/inc, -timeout, -save-db, -reindex, -no-result-error, -ipc1/2/3, journal options (-journal, -watch, -action-filter, -from-*/-to-*, -changed-today; 1.5) [confirmed: E]
- SDK: DLL+Lib over IPC/WM_COPYDATA, ANSI/Unicode, thread safe, blocking/non-blocking, x86/x64; examples in C/C++, C#, Clarion, VB, Python; search state, query, results, general, run history APIs [confirmed: K]; SDK3 in 1.5 [confirmed: A]
- URL protocol es: [confirmed: C (-url, -install-url-protocol)]; "search:" protocol [unverified]

### Removed from draft
- "dupe family: sizedupe:, namepartdupe:, dmdupe:" : not in current function list (only dupe:, dupe-count:, dupe-from:, dupe-max:, dupe-min:, distinct:, unique:). Likely older 1.4 aliases; mark as unverified legacy.
- "runcount:, daterun:" as names: current canonical names are run-count: and date-run: (old forms may be aliases, unverified).
- "fsi:", "shell:": not on current functions page; not confirmed.
- "content:/utf8content:/ansicontent:" : only content: on page; utf8/ansi/utf16 content are 1.5 alpha properties/variants, not confirmed as search functions.
- "len:" as a function: it is a modifier on the modifiers page (and F lists length: separately as duration).
- "macros (define custom functions)": no macro definitions on F ("No macros are defined"); the real feature is Filters/Bookmarks referenced as macros in es.exe (1.5), plus filter macros audio: doc: exe: image: video: zip:.
- "-search, -s, -path, -filter, -bookmark" listed vaguely: correct but expand (done above).
- "journal/results: sort by most-run" : exists as -select-most-run-result and sort by run-count, but "UI sort by most-run" is not separately documented.
- "root:" is real (confirmed in F). "infolder:, nosubfolders:" : not on F (use parent:/ancestor:/child:); unverified legacy.
- "url protocol es: / search: ?" resolved: es: confirmed via -url option; search: unverified.

## 2. WinDirStat (1.1.2 + 2.x)

Note: no source page compares 1.1.2 with 2.x; 1.1.2 items below come from prior knowledge and are [unverified]. Latest 2.x per GitHub API: v2.9.3 (2026-10-11), 2.9.2 (2026-10-06), 2.9.0 (2026-09-26).

### 1.1.2 baseline
- Scan chosen drives / folder; all local drives; cushion treemap; directory tree list; extension list with colors; cleanups menu; email report; open in Explorer; Recycle Bin empty; user-defined cleanups [unverified]
- Pacman progress animation (2.0.1) [confirmed: GC]

### Scan
- Scan multiple drives, folders, network shares, MTP devices; pause/resume; refresh [confirmed: GR]
- Accelerated local NTFS scanning when elevated; direct NTFS MFT scanning (2.5.0), cancel while loading MFT records (2.9.0) [confirmed: GR, GC]
- Multi-folder selection (2.9.0); double-click scan in Select Drives dialog (2.6.0) [confirmed: GC]
- Per-drive multithreaded scanning; long file names; reparse-point exclusions; backup/restore privileges; `\\?\Volume{GUID}` paths (2.2.2) [confirmed: GC]
- Hardlink tracking (2.5.0); sparse file attribute indicator (2.0.3) [confirmed: GC]
- Exclusions: by path (2.1.1), by name (2.1.1), by min size (2.1.1), include filters and file age filtering (2.6.0), greater/less-than comparisons for size and age (2.9.0), regex path filtering, filter exclusion context-menu actions (2.6.0), exclude Dropbox "ignored" items (2.9.0) [confirmed: GC, GR]
- Elevation: relaunch elevated, elevation eligibility detection and prompt (2.5.0), auto-copy drive mapping when admin [confirmed: GC]
- Previous-folder history with pruning (2.5.0/2.6.0) [confirmed: GC]
- Command-line scan options (2.5.0); /SaveTo, /SaveDupesTo, /SavePermsTo (write report and exit), /LoadFrom [confirmed: GRM, GC]
- Windows Server Core support; Windows 7/Server 2008 R2 through Win 11/Server 2025 [confirmed: GRM, GC]

### Views
- Directory tree list; columns chosen via right-click on header (column choosers) [confirmed: GRM, GC (2.8.0)]
- Columns confirmed by name: Owner (2.0.1), logical vs physical size (2.0.1), file extension info in duplicate list (2.2.1), Items/Files/Folders/Last change/Attributes/Subtree %/Percentage [unverified: no page enumerates these]
- Free space percentage beside volume label (2.1.1); free/unknown space labels in sunburst/flame graph (2.9.0) [confirmed: GC]
- Absolute/relative percentage display modes (2.8.0); option to show seconds in timestamps (2.9.0) [confirmed: GC]
- File types (extension) list; group unregistered extensions (2.7.0); extension search on double-click (2.7.0); auto-search from extensions menu (2.5.0) [confirmed: GC, GR]
- Cushion treemap; treemap style presets (2.9.0); Hilbert and Moore layouts (2.8.0); treemap folder frames option (2.7.0); customizable folder frame colors and draw threshold (2.8.0) [confirmed: GC]
- Sunburst view and flame graph view (2.8.0) [confirmed: GC]
- Largest Files tab (2.2.1) [confirmed: GC]
- Duplicates view (hash-based, selectable algorithms (2.6.0), xxHash XXH3 (2.7.0), sparse hashing for files over 64 MiB with "Probable duplicates" label (2.9.0), cloud-file safeguards) [confirmed: GC, GR]
- Search (Ctrl+F) by name, size range, item type, owner, regex; size totals for search results (2.9.0) [confirmed: GR, GC]
- Storage Analytics / cost optimization (tier cost by modification age) (2.7.0) [confirmed: GC, GR]
- Permissions scanning and view/export (2.7.0) [confirmed: GC]
- File Watcher (monitor file-system changes) with toolbar controls and live regex path filtering (2.8.0/2.9.0) [confirmed: GC, GR]
- Window layout chooser and additional layouts (2.7.0/2.8.0); size proportion tooltips; selected item count/stats and space sum for selection in status bar (2.0.3/2.7.0) [confirmed: GC]
- Multiple item selection (2.0.1) [confirmed: GC]
- Dark mode (2.5.0); configurable colors, fonts, toolbar size, larger toolbar icons, settings button on toolbar (2.6.0), font/toolbar scaling with Windows text size (2.9.0) [confirmed: GC, GR]
- Translations built in (2.0.1); added Korean, Traditional Chinese, Swedish, Japanese, Turkish, Arabic, Hindi [confirmed: GC]
- Process CPU and I/O priority configurable (2.8.0) [confirmed: GC]
- Portable settings via INI (2.0.1); MSI (x64/ARM64/x86), MSIX bundle, zip/7z portable, Store, winget, Chocolatey, Scoop; option to add to PATH (2.6.0) [confirmed: GRM, GC]

### Cleanups / actions
- Open, open with/Explorer context menu (full or legacy shell menu), PowerShell launch, right-click Explorer menu; per-user Explorer context menu registration (2.8.0); toggle Explorer menu from app [confirmed: GC]
- Move files (2.5.0); delete (to Recycle Bin / permanently) with confirmation prompt for destructive actions (2.8.0) [confirmed: GC (confirmation, move); recycle/permanent split [unverified]]
- Shortcuts to common cleanup utilities: Disk Cleanup, Programs and Features launcher, Microsoft Storage Settings (2.7.0) [confirmed: GC]
- NTFS transparent compression [confirmed: GC (2.0.1)]; individual algorithms LZNT1/XPRESS4K/8K/16K/LZX [unverified]
- Hardlink deduplication [confirmed: GR]
- Empty-folder cleanup / remove empty folders tool (2.1.1/2.7.0); set folder dates to last change (2.7.0) [confirmed: GC]
- Disable hibernation (hiberfil.sys) cleanup with fallback deletion (2.2.1/2.2.2) [confirmed: GC]
- VHD optimization (2.5.0); DISM analyze option (2.5.0) [confirmed: GC]
- Custom cleanup commands, unlimited (2.9.0); placeholders %p %n %s %d, recurse/ask/refresh options [unverified]
- Recover deleted files: NTFS and exFAT (2.9.0), FAT12/16/32 and original name/path for Recycle Bin recovery (2.9.2); needs admin [confirmed: GC, GR]
- Deletion warning and progress for profile deletion (2.7.0); progress dialogs for long operations; cancellable file hashing progress (2.8.0) [confirmed: GC]

### Export / reports
- CSV export (2.0.1); JSON save/load of scan results and duplicate lists (2.6.0); save duplicate lists to CSV (2.5.0); UTF-8 BOM support (2.7.0); permissions export; /SaveTo etc. [confirmed: GC, GRM]
- Email report (1.x) [unverified]

### Options
- Logical vs physical size, hardlink handling, hash algorithm selection, sparse hashing, duplicate scanning on/off, process priority, time seconds, percentage mode, treemap folder frames/colors, layouts, dark mode, portable INI [confirmed: GC, GR]
- Free space / unknown space show-hide, list grid/stripes [unverified]

### Removed from draft
- "NTFS MFT fast scan (2.x)" kept; "tabs: Largest files, Duplicates, Search" kept. "report/email (1.x)" kept as unverified, not claimed for 2.x.
- "save/load scan results (CSV)": loading via CSV not confirmed; load is JSON (2.6.0) and /LoadFrom; CSV is export/save.
- "Drives: skip hidden/protected" and "follow junctions option": replaced by confirmed reparse-point exclusions; hidden/protected [unverified].
- "Owner column (2.x)" confirmed as 2.0.1, not new in 2.x later.
- "Disk Cleanup launch": merged under shortcuts to cleanup utilities.
- "remove local copies of cloud files?": not confirmed (only cloud-file safeguards for hashing); dropped.
- "Rescan all / rescan selected": generic Refresh confirmed; per-branch rescan [unverified].
- "Windows 7 support": confirmed restored in 2.6.0.

---

# Part 2: Keel against Everything, indexing

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| NTFS MFT enumeration | Has | `crates/keel-search/src/ntfs/win.rs:262`; `crates/keel-search/src/ntfs/mod.rs:173` | —; uses `FSCTL_ENUM_USN_DATA` with elevation, not a raw-sector MFT parser. |
| USN journal live updates | Has | `crates/keel-search/src/ntfs/win.rs:291`; `crates/keel-search/src/ntfs/mod.rs:42` | —; polls every 2 seconds, including unprivileged journal reads after an elevated initial build. |
| ReFS indexing | Part | `crates/keel-search/src/ntfs/win.rs:52`; `crates/keel-core/src/library.rs:42`; `crates/keel-core/src/index.rs:563` | Generic library folder walks can index readable folders; native fast-volume discovery explicitly accepts only fixed NTFS drives. No ReFS-specific fast index. |
| FAT folder-scan indexing | Has | `crates/keel-core/src/library.rs:42`; `docs/library.md:9` | —; add a readable folder/drive as a library source; not an automatic whole-machine pane index. |
| exFAT folder-scan indexing | Has | `crates/keel-core/src/library.rs:42`; `docs/library.md:9` | —; generic library source walk. |
| Network-folder scan indexing | Has | `crates/keel-core/src/library.rs:46`; `docs/library.md:9` | —; UNC/share, SFTP and supported cloud sources. |
| Folder-index rescan schedule | Part | `crates/keel-core/src/index.rs:1224`; `crates/keel-core/src/library.rs:62`; `crates/keel-search/src/walkindex.rs:27` | Library has periodic reconciliation and configurable remote polling; walk index has a fixed seven-day age threshold. No Everything-style user-configurable schedule for each arbitrary local pane-index root. |
| Index file size | Has | `crates/keel-core/src/schema.sql:151`; `crates/keel-search/src/walkindex.rs:73`; `crates/keel-search/src/ntfs/db.rs:23` | — in library/walk index; native NTFS name index does **not** store size. |
| Index date modified | Has | `crates/keel-core/src/schema.sql:152`; `crates/keel-search/src/walkindex.rs:77`; `crates/keel-search/src/ntfs/mod.rs:359` | — in library/walk index; NTFS results fetch it on demand. |
| Index date created | Lacks | `crates/keel-core/src/schema.sql:134`; `crates/keel-search/src/ntfs/db.rs:23`; `crates/keel-search/src/walkindex.rs:73` | Library `ctime` is filesystem **change** time, explicitly no longer creation time. No native creation-time index field. |
| Index date accessed | Lacks | `crates/keel-core/src/schema.sql:145`; `crates/keel-search/src/ntfs/db.rs:23`; `crates/keel-search/src/walkindex.rs:73` | No access-time field in these index records. |
| Index file attributes | Part | `crates/keel-core/src/schema.sql:32`; `crates/keel-core/src/schema.sql:158`; `crates/keel-search/src/ntfs/usn.rs:62` | Library stores unreadable/hidden/link flags; NTFS retains only the directory bit. Full Windows attributes are not indexed. |
| Fast indexed folder sizes | Part | `crates/keel-core/src/schema.sql:317`; `docs/features.md:23` | Library caches and incrementally updates apparent-byte totals after walking; no MFT-derived size/ancestor aggregation in the NTFS index. Unindexed folders require a walk. |
| Fast sort per indexed column | Part | `crates/keel-core/src/schema.sql:167`; `crates/keel-search/src/ntfs/index.rs:82`; `crates/keel-core/src/search.rs:951` | Library has selected database indexes and UI sorting exists; no Everything-style prebuilt fast-sort indexes for every metadata column or requested backend sort in `Query`. |
| Index arbitrary Windows properties | Lacks | `crates/keel-core/src/schema.sql:145`; `crates/keel-search/src/lib.rs:61`; `crates/keel-search/src/everything.rs:178` | Fixed record/media fields; no arbitrary property-system index. Everything adapter requests only full path, size and modified date. |
| Content indexing | Has | `crates/keel-core/src/schema.sql:367`; `docs/library.md:77` | —; native library text extraction/FTS for supported documents. No OCR; source permissions/settings and extraction limits apply. |
| Include-list folders / selected index roots | Has | `crates/keel-core/src/library.rs:54`; `docs/library.md:9` | — via explicit library sources. Native MFT backend instead discovers all fixed NTFS volumes. |
| Exclude-list folders | Has | `crates/keel-core/src/library.rs:60`; `crates/keel-core/src/index.rs:569` | — via source-relative gitignore-style patterns in the library; no such configurable filtering in native MFT table. |
| Wildcard file exclusions | Has | `crates/keel-core/src/library.rs:60`; `crates/keel-core/src/index.rs:569` | — via library ignore patterns. |
| Include/exclude hidden files | Has | `crates/keel-core/src/library.rs:59`; `crates/keel-core/src/index.rs:565` | — per library source; not a native MFT inclusion setting. |
| Include/exclude system files | Lacks | `crates/keel-core/src/library.rs:54`; `crates/keel-core/src/schema.sql:32`; `crates/keel-search/src/ntfs/usn.rs:62` | No system-attribute inclusion switch/indexed flag. Path-pattern exclusions are only a workaround. |
| Offline-volume search | Has | `docs/library.md:11`; `crates/keel-core/src/library.rs:72`; `docs/library.md:95` | — through retained library source snapshots. NTFS backend initially loads currently enumerated fixed NTFS volumes (`ntfs/mod.rs:298`), not arbitrary disconnected DBs. |
| Database saved to disk | Has | `crates/keel-search/src/ntfs/db.rs:49`; `crates/keel-search/src/walkindex.rs:727`; `crates/keel-core/src/library.rs:1` | —; separate NTFS, walk-index and library SQLite stores. |
| Instant start from saved database | Part | `crates/keel-search/src/ntfs/mod.rs:291`; `crates/keel-search/src/walkindex.rs:843` | Saved data avoids rebuilding, but NTFS load explicitly takes seconds for millions of entries; walk index loads asynchronously. No verified instant-start latency guarantee. |
| Non-admin search/index service | Part | `crates/keel-search/src/ntfs/mod.rs:85`; `crates/keel-search/src/ntfs/mod.rs:100`; `crates/keel-search/src/ntfs/win.rs:291`; `docs/features.md:36` | Normal search and journal catch-up work without elevation; one-shot `--index-service` build requires UAC. `keel-daemon` serves the library, not a persistent privileged Everything Service equivalent. |
| Multiple instances | Has | `docs/files.md:142`; `docs/files.md:157` | —; `--new-window`, separate profiles, or disable running-window reuse. |
| Portable mode | Part | `crates/keel-app/src/settings.rs:249`; `crates/keel-app/src/settings.rs:262`; `crates/keel-search/src/ntfs/mod.rs:53`; `docs/features.md:42` | Environment overrides relocate settings/session/index/data, but `KEEL_CONFIG_DIR` alone leaves eframe window state in its own folder and does not relocate every data store. No single fully self-contained portable-mode switch. |


---

# Part 3: Keel against Everything, everything else


**Status convention.** Status covers Keel's **own** engines: the pane search's native backends (NTFS index `keel-search/src/ntfs`, walk index `walkindex.rs`, Spotlight/locate) plus library search (`keel-core/src/search.rs`). On Windows, when Everything.exe is running, the pane query string goes to Everything **verbatim** (`crates/keel-search/src/everything.rs:154-160`, `:176`). So every Everything function in this section works in the pane at that point, through Everything. Rows marked "E passthrough only" have no Keel-side implementation. Paths are relative to `keel/`. "Pane" means the System backend of a search tab. "Library" means the Library backend, `keel search`, the `search` API op, MCP and the web client.

### Search syntax

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Substring match by default | Has (pane) / Part (library) | `crates/keel-search/src/ntfs/pattern.rs:136-141` (`Term::Literal`); walk index same, `walkindex.rs:569`; library `crates/keel-core/src/search.rs:484-501` (FTS5 word-prefix), `:821-827` (leftover words as path `LIKE`) | Library matches word prefixes, not arbitrary substrings inside a word. Only the last term is a prefix (`text_parts`). Spotlight/locate use their own semantics. |
| Space = AND | Has (both) | `pattern.rs:73-87`, `ntfs/index.rs:122-123` (`rest` must all match); library `search.rs:470-475`, `content_match` joins with AND `:516-529` | — |
| `\|` = OR | Lacks | Not handled in `pattern.rs:17-36`. Library treats it as plain text (`docs/library.md:73`). | No OR on either path (E passthrough only). |
| `!` = NOT | Part | Library `-content:` only (`search.rs:462`, `:666-669`) | No general negation of a name/path term or filter on either path. |
| `< >` grouping | Lacks | — | No grouping or precedence anywhere (E passthrough only). |
| Quotes (phrase / literal with spaces) | Has (both) | `pattern.rs:23` (quote toggles); library `chips.rs:107-128` tokens, `search.rs:385-391` phrases | — |
| Wildcards `*` `?` | Has (pane) / Lacks (library) | `pattern.rs:38-56` whole-name glob, `:136-138`; walk index `walkindex.rs:536-550`; library treats `*` as plain text (`docs/library.md:73`) | No wildcard in library queries. |
| `regex:` | Has (pane) / Lacks (library) | `pattern.rs:82-83` (per-term) and `:68-71` (`Query::regex` whole text); `lib.rs:247-251` plain walk; `locate.rs:31` `--regex` | Library has no regex. The UI never sets `Query::regex` (`crates/keel-app/src/worker.rs:281-285`): only the `regex:` prefix works. |
| `case:` / Match Case | Lacks | `Query::match_case` exists (`crates/keel-search/src/lib.rs:38`), honoured by `pattern.rs:103`, but there is no `case:` term and the app never sets it (`worker.rs:281-285`). Library FTS `unicode61` is always case-insensitive (`crates/keel-core/src/schema.sql:54`). | Term plus toggle (E passthrough only). |
| `wholeword:` / `ww:` | Lacks | Library terms are word-prefix FTS matches (`search.rs:484-501`), not exact whole words | Exact whole-word option on either path. |
| `path:` (match against full path) | Part | Library bare words match name **or** path (FTS `path` column, `schema.sql:53-54`; `docs/library.md:50`) | Native NTFS/walk indexes match names only (`ntfs/index.rs:1-2`, `scan` over names). No explicit `path:` term. |
| `nopath:` | Part | Library `name:` restricts to names (`search.rs:463`, `:500`). Native is name-only already. | No `nopath:` keyword. Library default includes paths. |
| `diacritics:` | Part | Library always folds diacritics (`unicode61 remove_diacritics 2`, `schema.sql:54`) | No toggle. Native indexes do no folding (ASCII case folding only, `ntfs/index.rs:515`). |
| `file:` | Part | Library `file:` (`search.rs:458`) | Native NTFS/walk have only `folder:`. `file:` is a literal substring there. |
| `folder:` | Has (both) | `pattern.rs:74-79`; library `search.rs:459`; `Query::folders_only` → Everything `folder:` (`everything.rs:156-157`) | — |
| `ext:` | Part | Library `ext:pdf;docx` (`search.rs:398-403`, SQL `:625-631`) | Native NTFS/walk do not parse `ext:`, so it matches as a literal substring, yet the Windows hint advertises it (`crates/keel-app/src/search_tab.rs:18`). |
| `type:` (file type description) | Part | Library `type:` is an alias of `kind:` (image/video/audio/document/archive/file/folder, `search.rs:448-457`, `:45-93`) | No Windows type-description matching. Not available natively. |
| `size:` comparisons / ranges / units | Part | Library `>`, `>=`, `<`, `<=`, `=`, `a..b` (`search.rs:324-345`); units b/kb/mb/gb/tb (1024-based, `:231-247`) | Constants `empty/tiny/small/medium/large/huge/gigantic`. Native has no `size:` (the NTFS record has no size, `ntfs/db.rs:22-24`). |
| `dm:` date modified | Part | Library `dm:`/`datemodified:`/`modified:` (`search.rs:418-423`) | Native lacks it (NTFS index has no dates; the walk index has mtime, `walkindex.rs:73-78`, but no term). |
| `dc:` date created | Lacks | `record.ctime` is change time, not creation (Part 2) | No created-date index or term. |
| `da:` date accessed | Lacks | — | No access-time index or term. |
| `dr:` / `rc:` recently changed | Lacks | — | No "recently changed" timestamp (Everything's USN-observed change time). The NTFS USN loop doesn't record it (`ntfs/usn.rs`). |
| Date constants and ranges | Part | `today`, `yesterday`, `thisweek` (Mon), `thismonth`, `thisyear`, `YYYY`, `YYYY-MM`, `YYYY-MM-DD` (`-`, `/`, `.` separators), `a..b`, comparisons (`search.rs:278-345`) | `lastweek/lastmonth/lastyear`, `pastNdays`, month names, times of day, local-format dates. Library only. |
| `attrib:` | Lacks | Library stores only hidden/link/unreadable flags (`schema.sql:158`). NTFS keeps only the dir bit (`ntfs/index.rs:40-47`). | Attribute index and term. |
| `parent:` / `infolder:` / `nosubfolders:` | Part | Native `in:<path>` = recursive subtree prefix (`pattern.rs:80-81`, `ntfs/index.rs:407-414` `within`). Library `in:`/`source:` = source label or id only (`search.rs:469`, `:673-679`). | No direct-children-only filter. Library `in:` does **not** take a path, though `docs/library.md:67` says "under a path or source". Keel's `in:` is not Everything syntax, so it fails while Everything is the backend. |
| `depth:` / `parents:` | Lacks | — | Depth filter (native slots have parent links, `ntfs/index.rs:40-47`, so depth is computable). |
| `len:` (name length) | Lacks | — | — |
| `child:` / `childcount:` / `childfilecount:` / `childfoldercount:` | Lacks | Library keeps per-folder totals and file counts (`schema.sql:324`, `folder_total`) but no term | Terms. Native has no child counts. |
| `empty:` (empty folders) | Lacks | `folder_total` could answer it (`schema.sql:324`) | Term. |
| `root:` | Lacks | — | — |
| `startwith:` / `endwith:` | Part | Native globs `abc*` / `*abc` do the same (`pattern.rs:38-56`). The NTFS ranker prefers prefix matches (`ntfs/index.rs:511-530`). | No keywords. Library has no wildcard and no suffix match. |
| `dupe:` / `sizedupe:` / `namepartdupe:` / `dmdupe:` and the other `*dupe:` | Lacks | Closest: library content-hash duplicate finder (`docs/library.md:29`; API `duplicates`, `crates/keel-api/src/ops.rs:109`) | No name/size/date duplicate search functions. Content dupes are a separate view, not a query term. |
| `content:` | Has (library) | `search.rs:461` (FTS over extracted text, `schema.sql:367-382`); also bare words (`:516-529`); `-content:`, `contents:off` (`:462-468`) | Native pane search has no content search. Supported formats only, no OCR (`docs/library.md:73-77`). |
| `utf8content:` / `ansicontent:` (and other encodings) | Lacks | — | Encoding-specific raw content scan. |
| `runcount:` / `daterun:` | Lacks | Library `opened(source, record, ts)` keeps only the last open time (`schema.sql:102-103`); special query `is:recent` (`crates/keel-app/src/library.rs:33`) | Run counter and query terms. |
| `fsi:` (volume index) | Part | Library `in:`/`source:` selects a source (`search.rs:469`) | No native volume selector (the NTFS backend searches all volumes, `ntfs/mod.rs:343-345`). |
| `frn:` | Lacks | The NTFS index is keyed by FRN internally (`ntfs/db.rs:24`, `ntfs/index.rs:40`) | Query term. |
| `shell:` (known folders) | Lacks | — | — |
| `width:` / `height:` | Has (library) | `w:`/`width:`, `h:`/`height:` (`search.rs:433-441`) | Needs the sidecar media job. Native lacks. |
| `dimensions:` / `orientation:` / `bitdepth:` | Lacks | — | Terms. `dimensions:WxH` and orientation are derivable from the stored w/h; bit depth isn't stored. |
| Audio tags `artist:` `album:` `title:` `genre:` `track:` `year:` `comment:` | Lacks | Media rows hold camera/taken/w/h/duration/gps only (`search.rs:583-608`) | Audio tag extraction and terms. |
| `length:` (audio/video duration) | Part | Library `duration:`/`length:` with ms/s/m/h (`search.rs:442-445`, `:347-362`) | Docs say videos only (`docs/library.md:63`). Audio duration isn't extracted. |
| Macros (user-defined functions) | Lacks | — | — |
| Search modifiers that combine (e.g. `case:regex:`, `nopath:ww:`) | Lacks | Only `regex:` and `folder:` exist natively (`pattern.rs:74-85`) | Modifier framework. |
| Library-only extras (not in Everything) | n/a | `tag:`, `camera:`, `taken:`, `has:gps`, `kind:photo`, `name:`, `contents:` (`search.rs:424-469`); `is:favorite`/`is:recent` (`keel-app/src/library.rs:32-33`) | — |

### UI

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Search as you type | Has | 150 ms debounce (`crates/keel-app/src/search_tab.rs:11`, `:124-127`); stale queries dropped (`worker.rs:262-301`) | — |
| Match highlighting | Part | Library content snippets highlight matched words (`chip_row.rs:77`, `view_details.rs:250-312`) | Names and paths in results aren't highlighted on either path. Ctrl+P doesn't highlight matched characters either (`jump.rs:494-505`). |
| Toggles: Match Case / Whole Word / Match Path / Diacritics / Regex | Lacks | The search bar is only a backend combo box plus a text box (`search_tab.rs:64-118`) | All five toggles. `Query::{regex, match_case}` exist but aren't wired (`worker.rs:281`). |
| Built-in filters (Everything/Audio/Compressed/Document/Executable/Folder/Picture/Video) | Part | Library **Kind** chips Image/Video/Audio/Document/Archive/Folder (`crates/keel-core/src/chips.rs:12-23`, `docs/library.md:35`) | No Executable filter. None for the System backend. |
| Custom filters | Lacks | — | User-defined named filters (macro + search + columns). |
| Filter bar / sidebar (1.5) | Part | Library chip row (Kind/Type/Tag/Source/Size/Modified/Favorites/Contents) (`crates/keel-app/src/chip_row.rs:25`, `search_tab.rs:139-155`) | Library backend only. No filter sidebar. |
| Bookmarks (search + filter + sort + index) | Part | Library saved views (`name`, `query`, `layout`): `crates/keel-core/src/tags.rs:634-685`, `schema.sql:104`; listed in the sidebar (`keel-app/src/library_ui.rs:238-241`); API `views.list` (`keel-api/src/ops.rs:87`) | No UI or API to **create**, edit or delete a view (only `create_view` in core, used by tests `tags_tests.rs:243`). No sort is stored. No bookmarks for System-backend searches. |
| Search history + suggestions | Lacks | — | No query history, recent searches or autocomplete. |
| Result columns configurable | Part | Search tab Details columns: Name, Folder, Size, Modified (+ Copies, Tags with the library) (`view_details.rs:108-141`); resizable | Fixed set: no Date created/accessed, Attributes, Type, Run count, Date run, Recently changed. Folder size shows in the Size column when known (`docs/files.md:110`). No column chooser. |
| Status bar: result count + selected size | Has | `search_tab.rs:150-156` ("first 500 results"); status bar items, selected count, bytes (`crates/keel-app/src/app.rs:629-660`) | — |
| Thumbnails / large icons view | Has | Grid view with thumbnails (`crates/keel-app/src/pane.rs:247`, `:370`) | Media view is disabled for search tabs (`media_viewer.rs:254`). |
| Preview pane (1.5) | Has | `crates/keel-app/src/preview_panel.rs` | — |
| Tabs (1.5) | Has | Search is a tab kind (`crates/keel-app/src/tab.rs:51-59`); several search tabs at once | — |
| Multiple windows | Has | `--new-window`, profiles (`docs/files.md:148`, `:157`) | — |
| Global hotkey: new window / toggle | Part | Hotkey brings Keel forward (`crates/keel-app/src/hotkey.rs:1-7`, `docs/files.md:157`) | No "new search window" or show/hide toggle action. No hotkey that opens a search tab directly. |
| Tray icon | Lacks | — | — |
| Shell context menu | Has | Windows shell menu via More options, Shift+right-click or Shift+F10 (`docs/files.md:51-57`) | `docs/parity.md:164` still calls it missing (stale). |
| Drag-drop out | Has | `crates/keel-app/src/dragout.rs`; `docs/files.md:30-36` | — |
| F2 rename | Has | `crates/keel-app/src/keys.rs:37-38` (inline rename), `:318` (Ctrl+F2 bulk); search tabs relist after a rename (`state.rs:885-890`) | — |
| Copy full path | Has | `keys.rs:99` `CopyPath` | — |
| Open path (open containing folder) | Has | `Action::OpenLocation`, Ctrl+Enter (`search_tab.rs:94-96`, `state.rs:2270`) | — |
| Properties | Has | `crates/keel-app/src/dialogs/properties.rs`; `keys.rs:120-122` (incl. Explorer's sheet) | — |
| Delete | Has | `keys.rs:57` (Recycle Bin, confirm) | — |
| Run count tracking | Part | Library `opened` last-open timestamp and Recents (`schema.sql:102`, API `recents`/`recents.note` `ops.rs:99-101`) | No counter. Library-indexed files only. |
| Keyboard shortcuts configurable | Part | Global hotkey is configurable (`docs/files.md:157`) | All other shortcuts are a compile-time const table (`keys.rs:275`). `docs/parity.md:43` overstates this as "configurable keys". |
| Localization | Lacks | `docs/parity.md:44` "English only" | — |
| Dark mode (1.5) | Has | Color themes and dark/light toggle (`docs/files.md:163`, `app.rs:687-694`) | — |

### File lists

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| `.efu` create / edit / open / search | Lacks | No EFU code (grep `efu` returns nothing) | Whole feature. |
| Export results as EFU / CSV / TXT / M3U | Part | `keel search <q> [--max N] [--json]` prints size + path text or JSON (`crates/keel-app/src/commands.rs:396-410`, `docs/api.md:427`) | Library search only. No export from a search tab. No EFU/CSV/M3U format. |

### Remote

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| ETP / FTP server | Lacks | Keel is an FTP/FTPS **client** only (`docs/features.md:21`) | Serving the index over ETP/FTP. |
| HTTP server (web UI search + download) | Has | `keel-daemon --web` serves search, preview and one-time download links (`docs/devices.md:43-55`; `keel-web/src/app.rs:1108-1113`; API `file.get` `ops.rs:77`) | Searches the **library** only, not a whole-disk name index. Loopback by default, token required. |
| 1.5 Everything Server / client | Part | `keel-daemon` owns the library; window, CLI and MCP attach as clients over a per-user pipe/socket or a token-guarded WebSocket (`docs/daemon.md:11`, `docs/library.md:79`) | Serves library search only. The pane's NTFS/walk index isn't exposed or shareable. |
| Network index sharing | Part | Paired devices: `node://` browse, a device's source can be added as a library source and searched (`docs/devices.md:19`); library sync syncs tags and content ids, not the name index | No shared index across machines. Each library indexes its own sources. |

### Automation

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| `es.exe`-style command-line search | Part | `keel search <query> [--max N] [--json]` (`docs/api.md:427`, `commands.rs:396`) | Library only, so no whole-disk name search from the CLI. No sort, columns, offset or path filter flags. |
| SDK / IPC API | Part | JSON-RPC ops over a pipe or WebSocket (`docs/api.md:312-320`); `search` op (`ops.rs:79`); MCP `keel mcp`, one tool per op (`crates/keel-api/src/mcp.rs:261`, `docs/daemon.md:36-45`) | The pane backends (Everything/NTFS/walk) have no API or MCP op. |
| Command-line options (`-search`, `-s`, `-path`, `-filter`, `-bookmark`) | Part | `keel [FOLDER] --search QUERY` opens a search tab (`docs/files.md:142-150`, `crates/keel-app/src/cli.rs:30-32`, `:445`) | The search isn't scoped to FOLDER (pane search is global, `state.rs:1021-1069`). No filter, bookmark or backend selection flags. |
| URL protocol (`es:` / `search:`) | Lacks | No scheme registration found | Whole feature. |

### 1.5 items

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Advanced multi-file rename (regex) | Has | Ctrl+F2 bulk rename with placeholders, find/replace (regex), case, conflict preview, undo (`crates/keel-app/src/bulk_rename.rs:1-3`, `:34`, `:81`) | — |
| Find duplicates UI | Part | Library duplicate finder by BLAKE3 content (`docs/library.md:29`, API `duplicates`) | Only indexed library sources. No name/size/date dupes view. |
| Journal (index change history) | Part | Library `op_log` of Keel's own operations, shown per item in the Inspector (`schema.sql:21`, `docs/library.md:27`) | No log of filesystem/index changes. USN changes are applied but not recorded (`ntfs/usn.rs`). |
| Result omission | Lacks | — | Hiding chosen results or paths from results. |
| Sort by most-run | Part | Recents = most recently opened (`ops.rs:99`) | No count, so no most-run order. |
| Content indexing | Has | See Part 2; library text FTS (`schema.sql:367-382`) | No OCR. |
| Folder size indexing | Has | Library `folder_total` (`schema.sql:324`); see Part 2 | Library sources only. |
| Properties of any file (index/search/columns) | Part | Fixed media properties (camera, taken, w/h, duration, gps) via the sidecar job (`search.rs:133-185`) | No arbitrary Windows property system columns or terms. |

## Planner facts

### Query paths
- **Pane search (System backend).** `keel_search::default_searcher()` (`crates/keel-search/src/lib.rs:160`) picks the backend:
  - **Windows:** `Composite` (`composite.rs`) uses Everything while it runs (re-probed every 30 s) and otherwise `NtfsSearcher`. Without an index dir it falls back to the walk index, then to `Unavailable`.
  - **macOS:** Spotlight `mdfind -onlyin /` (`spotlight.rs`).
  - **Linux:** `plocate`/`locate -i [--regex]` (`locate.rs`).
  - **Fallback for all:** `WalkIndexSearcher` over home, then a plain `walk` per query.
  - Remote tabs (SFTP/cloud) do a breadth-first case-insensitive substring walk instead (`keel-app/src/remotes.rs:1026-1095`).
  - Interface: `trait Searcher { query(&Query) -> Vec<Hit> }`. `Query { text, folders_only, max (default 500), regex, match_case, meta }` (`lib.rs:33-56`). `Hit { path: VPath, is_dir, size, modified, snippet }` (`lib.rs:61-67`).
- **Everything adapter.** It sends the text unchanged, adding only a `folder:` prefix when `folders_only` is set. It requests full path, size and date modified only (`everything.rs:154-185`), so no other Everything columns reach Keel.
- **Native parser (NTFS and walk index, two copies).**
  - Where: `ntfs/pattern.rs:64-134` and `walkindex.rs:569-640`.
  - Tokenizing: whitespace splits terms and `"` toggles quoting (`split_terms`).
  - Term kinds: `folder:[x]`, `in:<path>` (subtree), `regex:<re>` (case-insensitive prefix match on the keyword). A term containing `*`/`?` becomes an anchored whole-name glob regex. Anything else is a literal substring (regex-escaped).
  - Matching: all terms are ANDed. The longest literal is the "lead" and is regex-scanned across one `\n`-joined name buffer, in parallel above 1 MiB (`ntfs/index.rs:295-357`). The other terms are checked per candidate.
  - Not supported: OR, NOT, grouping, `ext:`, size/date filters, path matching.
- **Library search.**
  - Parsing: `LibraryQuery::parse(s, utc_offset)` (`crates/keel-core/src/search.rs:379-478`) tokenizes with `chips::tokens` (whitespace, `"` quoted, `chips.rs:107`). A token is a phrase, or it's split on the first `:` into `key:value` (key lowercased). Unknown keys become plain terms. Errors only on a bad size, date, kind, duration or `contents:` value.
  - Output: `LibraryQuery` struct (`search.rs:133-185`).
  - Execution: FTS5 `record_fts(name, path)` with the unicode61 tokenizer and diacritics removed. Every word is quoted, so user text is never an FTS operator. Only the last word is a prefix. Filters become SQL over `record` (`filters()` `:612-671`). Content text goes through `record_content` FTS.
  - Source selection: per-source stores are searched one by one (`Library::search` `:1209-1247`), skipping sources that don't match `in:`.
  - Chips: `chips.rs` toggles terms inside the query string ("the box stays the truth").
  - Adapters: `LibrarySearcher` (`search.rs:1285`) and `keel-app/src/library.rs:1555-1570` (`LibSearch`, special `is:favorite`/`is:recent`) wrap it as a `Searcher` so the search tab can switch backends.

### NTFS index record (what it holds)
- In memory, `Slot { frn, parent, at, len, is_dir }`, with names in one `\n`-separated `String` (`ntfs/index.rs:39-65`). On disk, SQLite `files(frn PK, parent, name, is_dir)` plus `meta(journal_id, next_usn, root, volume)` (`ntfs/db.rs:22-26`).
- **No size, dates or attributes.** Hits are statted afterwards with `GetFileAttributesExW` when `meta` is on and hits ≤ 2,000 (`ntfs/mod.rs:49`, `:356-362`; `lib.rs:269-299`).
- Walk index entries do carry `{is_dir, size, mtime(ms)}` (`walkindex.rs:72-88`) but expose no filter on them.
- Library `record`: `id, parent, name, path, kind, size, mtime(ns), ctime(change), fs_id, cas_id, sampled_hash, gen, flags, error` (`schema.sql:145-159`), plus `media`, `folder_total`, `record_text`, tags and `opened`.

### Rendering, sorting, capping
- **Cap.** The search tab always asks for 500 (`search_tab.rs:12`, `worker.rs:281-285`) and shows "first N results" at the cap (`search_tab.rs:150-155`). There is no paging or "load more". Library `DEFAULT_MAX` is 500 (`search.rs:14`). The CLI/API take `max`. Ctrl+P's folder list asks for 200,000 (`lib.rs:220`).
- **Backend order:**
  - NTFS: rank by quality (0 exact, 1 prefix, 2 other, ASCII case-fold), then case-insensitive path. The prefix pass runs first, so exact and prefix matches aren't crowded out by the cap (`ntfs/index.rs:83-87`, `:407-430`, `:511-530`; merged across volumes `ntfs/mod.rs:343-348`).
  - Walk index: same (`walkindex.rs:405-479`).
  - Library: bm25 plus a 2.0 name boost per word plus a recency boost (30-day half-life); content-only hits rank after name hits (`search.rs:17-30`, `:801-840`). Sources are merged by score, then truncated (`:1227-1228`).
  - Everything: its own default order.
- **Display.** `hits_listing` turns hits into `Entry` rows (name = full display path, ext, size, modified; `tab.rs:601-629`). Details view in a search tab: Name | Folder | Size | Modified (+ Copies/Tags) (`view_details.rs:108-141`). Header clicks re-sort the ≤500 rows client-side (`view_details.rs:435-437`, `SortKey` Name/Size/Modified/Ext `tab.rs:41-46`). Library hits with text show a snippet line with marks (`view_details.rs:250-312`). The status bar shows the backend name and state (`app.rs:702-703`, `index_ui.rs`).

### Where the search UI lives (keel-app)
- `TabKind::Search { query, due, req }` (`tab.rs:53-59`). `Tab::search(dir)` (`tab.rs:195`), opened by `Action::Search`/Ctrl+F (`state.rs:2258-2270`, `keys.rs:301`).
- Query bar plus the System/Library combo (`search_tab.rs:51-157`, `tab.library_search`), drawn in place of the nav and filter bars (`pane.rs:212-218`).
- Dispatch: `AppState::start_search` (`state.rs:1021-1071`) → `SearchWorker` (one thread, stale requests dropped, `worker.rs:260-301`) → `Msg::Search` → `AppState::searched` (`state.rs:987-1018`).
- No separate search window. Ctrl+P (`jump.rs`) is a separate fuzzy folder/file popup over the backend folder list, a walk of the current tree and library names.

### Bookmarks, saved views, history (existing)
- **Saved views:** library table `view(id, name, query, layout)` (`schema.sql:104`); core CRUD `tags.rs:633-685` (create/update validate the query parses). They're listed in the sidebar's library "Views" section and open as library search tabs (`library_ui.rs:238-241`, `keel-app/src/library.rs:157`). API has only `views.list` (`ops.rs:87`). There's no create/edit UI or op.
- **Favorites and Recents:** library tag id 1 and the `opened` table. Query aliases `is:favorite`/`is:recent` (`keel-app/src/library.rs:32-33`). Ops `favorites.list`, `recents`, `recents.note`.
- **Quick access:** a fixed sidebar list (Home plus known folders, `sidebar.rs:48-72`). Users can't pin to it.
- **Tab back/forward history** exists for folders (`state.rs:3608`). There's **no search-query history.** Search tabs persist only as session tabs.
- **Chip filters persist only in the query text.** No per-profile saved filters.

---

# Part 4: Keel against WinDirStat


## Scan

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Choose a folder to scan | Has | `docs/files.md:113,117`; `crates/keel-app/src/state.rs:1214-1232` (Calculate sizes on the pane folder); `crates/keel-app/src/commands.rs:325-385` (`keel du <path>`) | None. Any folder can be scanned. The scan covers the subfolders of the folder shown in the pane. |
| Choose a drive to scan | Part | `docs/features.md:24` ("what fills a folder or a drive"); `crates/keel-app/src/sizes.rs:161-169` | Done by browsing to a drive root and opening Sizes. There's no drive picker. Removable and network drives only scan on request (`view_details.rs:87-96`). |
| Scan several folders/drives at once | Lacks | `crates/keel-app/src/state.rs:1214-1232` (one pane folder per job) | No multi-root scan and no combined result tree. The library can hold several sources, but there's no merged Sizes root across them. |
| Scan all local drives | Lacks | `crates/keel-app/src/sidebar.rs:10-11,138-165` (the drive list shows only free/total space) | No "all local drives" scan. The workaround is to add each drive as a library source (`docs/library.md:9`). |
| Multithreaded scan | Lacks | `crates/keel-vfs/src/sizes.rs:40-82` (recursive serial walk); `crates/keel-app/src/jobs.rs:300-324` (folders walked one after another); `docs/performance.md:91` (the library's first walk lists one folder at a time) | No parallel directory enumeration, either in Calculate sizes or in the library walk. |
| NTFS MFT fast scan | Lacks | `crates/keel-search/src/ntfs/*` (MFT/USN name index only); prior audit Part 2 ("native NTFS name index does not store size") | The MFT enumeration doesn't collect sizes and doesn't roll up folder totals. Sizes always come from a directory walk or the library index. |
| Follow junctions/mount points (option) | Lacks | `docs/files.md:119` ("links ... are neither counted nor followed"); `crates/keel-vfs/src/sizes.rs:52-54`; `crates/keel-core/src/totals.rs:1-6` | Links are never followed, and there's no option to follow them. |
| Skip hidden files (option) | Part | `crates/keel-core/src/library.rs:59` (`SourceDef.include_hidden`); `docs/files.md:119` | Only per library source. A Calculate sizes walk counts hidden files and has no switch for them (`vfs/sizes.rs:50-79`). |
| Skip protected/system files (option) | Lacks | `crates/keel-vfs/src/entry.rs:12-25` (Entry has no system attribute) | No system-attribute filter anywhere. |
| Hard-link aware | Lacks | `docs/files.md:119` ("A file with several hard links counts once for each path"); `crates/keel-core/src/totals.rs:6`; `crates/keel-vfs/src/sizes.rs:2` | Totals count each path. Identity data exists (`record.fs_id` = volume serial + file id, `crates/keel-core/src/fsid.rs:1-3`) and the duplicate finder already dedupes hard links by it (`crates/keel-core/src/hash.rs:1126-1145`), but totals don't use it. |
| Exclusion/scan filters (regex) | Part | `crates/keel-core/src/library.rs:60-61` (gitignore-style `ignore` per source); `crates/keel-core/src/index.rs:569` | Glob-style patterns for library sources only, with no regex. Calculate sizes and `keel du` walks take no filters. |
| Elevation | Part | `docs/files.md:92` (one-shot `--index-service` UAC for the name index); `docs/files.md:135` ("Keel never elevates") | A size scan never elevates. Unreadable folders count as empty and the job reports how many (`jobs.rs:325-330`). |
| Rescan all | Has | `crates/keel-app/src/pane.rs:735-741`, `palette.rs:137` (Calculate sizes with `fresh: true` drops and re-walks every subfolder); `crates/keel-app/src/library_ui.rs:212` (library "Index now") | None. |
| Rescan selected branch | Part | `crates/keel-app/src/sizes.rs:61-75` (`wanted` takes all entries of the tab) | Re-walks every subfolder of the shown folder, not just the selected one. To rescan one branch you have to navigate into it. |
| Show time taken | Lacks | `crates/keel-app/src/jobs.rs:21-29,568-584` (a job stores no start/end time to display) | No elapsed or total scan time is shown. |
| Pacman-style progress | Part | `crates/keel-app/src/jobs.rs:563-567` (progress bar plus percentage); `jobs.rs:300-303` (progress counts top-level subfolders done, with the current folder name) | Only a plain progress bar, and it advances one top-level subfolder at a time (coarse), with no file/byte counter while walking. |

## Views: directory list and columns

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Directory tree list (expandable) | Part | `crates/keel-app/src/view_details.rs:1-3,56-67`; `docs/files.md:117` (Enter goes down, Backspace up) | Sizes view is a flat list of one folder. You navigate into folders; there's no expandable tree with indented children. |
| Subtree percentage bar | Has | `crates/keel-app/src/view_details.rs:331-335,468-478` (`share_bar`, share of the parent folder) | None. |
| Bar colour by depth | Lacks | `view_details.rs:469-476` (always the accent colour) | Depth colouring isn't possible without a tree. |
| Column: Name | Has | `view_details.rs:137-142,248-317` | None. |
| Column: Subtree % | Has | `view_details.rs:150-151` (header "Share"), `331-335` | None. The bar shows a 0-decimal percentage. |
| Column: Percentage (of total/drive) | Has | `view_details.rs:188-192,368-376` ("Of used": share of the drive's used bytes) | It's relative to the drive's used space, not to the scan root. Empty on hosts and cloud. |
| Column: Size (logical) | Has | `view_details.rs:340-346`; `crates/keel-app/src/sizes.rs:151-159`; `docs/files.md:119` (apparent sizes) | None. |
| Column: Size on disk (allocated/compressed) | Lacks | `crates/keel-vfs/src/entry.rs:12-25` (only `size`); `crates/keel-core/src/schema.sql:145-160` (record has only `size`); no `GetCompressedFileSize`/`st_blocks` anywhere | No allocated size in Entry, the walk or the index. |
| Column: Items | Lacks | `view_details.rs:184-187` | No count of files plus folders. |
| Column: Files | Has | `view_details.rs:184-187,351-367`; `crates/keel-core/src/totals.rs:14-19` (`Total.files`) | None. |
| Column: Folders | Part | `crates/keel-app/src/inspector.rs:276`; `crates/keel-core/src/inspect.rs:266-276` (recursive CTE, library only) | Only in the Inspector, for one folder in a library source. It isn't stored in `folder_total` and isn't a column. |
| Column: Last change | Has | `view_details.rs:347-350` (Modified) | Shows the item's own mtime. WinDirStat shows the newest mtime in the subtree, which Keel doesn't. |
| Column: Attributes | Lacks | `crates/keel-app/src/dialogs/properties.rs:22-23,202-203` (hidden/read-only in Properties only) | No attributes column. |
| Column: Owner | Lacks | No owner lookup for listings (the only `GetSecurityInfo` is in `keel-api/src/private.rs:256`, used for socket ACLs) | No owner column. |
| Free space pseudo item | Lacks | `crates/keel-app/src/sidebar.rs:152-165` (free/total only in the sidebar) | No `<Free space>` row in Sizes. |
| Unknown space pseudo item | Lacks | `view_details.rs:67` (the drive's used space is read but never compared with the scanned sum) | No `<Unknown>` row (drive used minus scanned). |

## Views: extension list, treemap, tabs, UI

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Extension list: per-type rows | Lacks | `crates/keel-core/src/chips.rs:360-362` and `crates/keel-app/src/library.rs:1181-1192` (`top_extensions` counts names for search chips only) | No file-types pane. |
| Extension list: colour swatch | Lacks | None | No swatch. |
| Extension list: description | Lacks | None | No type description. |
| Extension list: bytes | Lacks | None | No bytes per type. |
| Extension list: % | Lacks | None | No percentage per type. |
| Extension list: file count | Lacks | Partial building block: `top_extensions` counts names, not bytes | No file count per type in a view. |
| Cushion treemap | Lacks | `docs/files.md:117` ("There is no treemap."); `crates/keel-app/src/pane.rs:14-27` (ViewMode has Details/Grid/Columns/Media/Sizes) | No treemap at all. |
| Treemap: brightness | Lacks | None | Option absent. |
| Treemap: cushion height | Lacks | None | Option absent. |
| Treemap: scale factor | Lacks | None | Option absent. |
| Treemap: light source | Lacks | None | Option absent. |
| Treemap: grid | Lacks | None | Option absent. |
| Treemap: colour per extension | Lacks | None | Option absent. |
| Treemap: highlight on select | Lacks | None | Option absent. |
| Treemap: zoom in/out | Lacks | None | Option absent. |
| Selection sync tree, treemap and extensions | Lacks | None | No linked views. |
| Tab: Largest files (top N) | Part | `crates/keel-core/src/schema.sql:172` (`record_size` index on file size); `docs/library.md:39,57` (library search `size:>1gb` filter); `crates/keel-app/src/tab.rs:741-760` (sort by size within one folder) | No top-N largest files across a tree. Library search filters by size but ranks by relevance, not size. |
| Tab: Duplicates (hash-based) | Has | `docs/library.md:17,29`; `crates/keel-core/src/hash.rs:1117-1170` (BLAKE3 `cas_id`, hard links dedupe); `crates/keel-app/src/library_ui.rs:999-1060` (Duplicate finder window, "Keep this one", "Open folder", reclaimable bytes); `palette.rs:144` | Library sources only, and only files already hashed. It's a separate window, not a tab of the size scan. |
| Tab: Search (name/regex) | Has | `docs/files.md:90,94` (substring, glob, `regex:`, `folder:`, `in:`); `crates/keel-app/src/search_tab.rs:18` | None. Search is a separate tab, not tied to scan results. |
| Dark mode | Has | `docs/files.md:165` (dark, light, Midnight, Paper, Dusk, High contrast, VS Code themes) | None. |
| Translations | Lacks | `docs/parity.md:221` (ranked gap #20 "Interface translations"); no i18n crate | English only. |

## Actions and cleanups

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Open | Has | `crates/keel-app/src/pane.rs:625` | None. |
| Open with | Has | `pane.rs:626-642`; `docs/files.md:16`; `crates/keel-app/src/keys.rs:103,116` | None. |
| Show in Explorer | Has | `pane.rs:727-732` ("Show in system file manager"); `state.rs:1824-1830` | None. |
| Command prompt / PowerShell here | Has | `pane.rs:733`; `docs/files.md:77-84` (internal terminal pane: PowerShell, Windows PowerShell, cmd, WSL); `state.rs:1832-1836` | Opens Keel's docked terminal, not a separate console window. |
| Copy path | Has | `pane.rs:669`; `state.rs:1755-1765` (several paths, one per line) | None. |
| Properties | Has | `pane.rs:742`; `crates/keel-app/src/dialogs/properties.rs:1-30,218-219` (Keel dialog plus "Windows Properties…") | None. |
| Delete to Recycle Bin | Has | `pane.rs:683`; `crates/keel-app/src/jobs.rs:350-432`; `docs/getting-started.md:172` | None. Undo is supported. |
| Delete permanently | Part | `docs/files.md:71` (only inside the Recycle Bin view); `crates/keel-app/src/keys.rs:434-435` (Shift+Delete sends to the trash) | No direct permanent delete for local files ("Keel never deletes local files for good on its own", `getting-started.md:172`). |
| Refresh selected | Part | `pane.rs:737-741` (Calculate sizes, fresh); `pane.rs:356` (F5 refreshes the listing) | Refreshes the whole shown folder, not the selected item only. |
| Empty Recycle Bin | Has | `docs/files.md:71`; `jobs.rs:262-279` (`TrashOp::Empty`) | Only from the Recycle Bin tab. No shortcut from the size view. |
| NTFS compression: LZNT1 | Lacks | Searched for `FSCTL_SET_COMPRESSION`/`compact`: no hits outside the search index's own compaction | Absent. |
| NTFS compression: XPRESS4K | Lacks | Same as above | Absent (WOF `FSCTL_SET_EXTERNAL_BACKING` not used). |
| NTFS compression: XPRESS8K | Lacks | Same as above | Absent. |
| NTFS compression: XPRESS16K | Lacks | Same as above | Absent. |
| NTFS compression: LZX | Lacks | Same as above | Absent. Zip/7z/tar archiving does exist (`docs/files.md:63`; `pane.rs:706-718`), but that's a different feature. |
| Remove local copies of cloud files | Lacks | Searched for `dehydrat`/`CfDehydrate`: no hits | Absent. Placeholders are only counted by their stated size (`docs/files.md:119`). |
| Optimize VHD | Lacks | None | Absent. |
| User-defined cleanup: custom command | Lacks | Searched for user command/tool config: none (`placeholder` hits are bulk rename and icon themes only) | No user-defined commands. Partial substitute: the Windows shell menu (`docs/files.md:53-57`) and palette `Shell:` entries run installed handlers. |
| User-defined cleanup: placeholders %p %n %s %d | Lacks | None | Absent. |
| User-defined cleanup: recurse option | Lacks | None | Absent. |
| User-defined cleanup: ask option | Lacks | None | Absent. |
| User-defined cleanup: refresh after | Lacks | None | Absent. |
| User-defined cleanup: files/folders/drives scope | Lacks | None | Absent. |
| Disk Cleanup launch | Lacks | Searched for `cleanmgr`/"Disk Cleanup": no hits | Absent. It can still be reached through the drive's "Windows Properties…" sheet (`properties.rs:218`). |
| Attributes operations (related) | Has | `docs/files.md:130` (Attributes…: hidden/read-only, octal modes on unix, undoable plan); `pane.rs:694` | Not recursive. No system/archive/compressed bits. |

## Export

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Save scan results (CSV) | Part | `crates/keel-app/src/commands.rs:412-445` (`keel du <path> --json`); `docs/api.md:114,428` (`library.folder_sizes`) | JSON for one level from the CLI/API only. No CSV, no whole-tree export, no save from the UI. |
| Load scan results | Lacks | `crates/keel-app/src/sizes.rs:13-15,25-36` (walked totals kept in memory for the session only) | Walked totals are lost on exit and can't be loaded back. Library totals persist in the index, which covers part of this. |
| Report/email | Lacks | None | Absent. |
| Copy to clipboard | Part | `state.rs:1755-1765` (Copy path) | Copies paths only, not the size listing. |

## Options

| Feature | Status | Evidence | Missing |
|---|---|---|---|
| Logical vs physical size | Lacks | `docs/files.md:119` (apparent sizes only) | No allocated-size mode (see "Size on disk"). |
| List grid/stripes | Part | `view_details.rs:102` (`.striped(true)`, hard-coded) | Not configurable. No grid lines. |
| List colours | Part | `docs/files.md:165-182` (themes recolour lists); `crates/keel-app/src/folder_style.rs` (per-folder colours in library sources) | No size-view-specific colour options. |
| Treemap style presets | Lacks | None | No treemap. |
| Show/hide free space | Lacks | None | No pseudo item to toggle. |
| Show/hide unknown | Lacks | None | No pseudo item to toggle. |
| Show/hide file types | Lacks | None | No extension pane. |
| Show/hide treemap | Lacks | None | No treemap. |
| Auto-scan setting (Keel-specific) | Has | `crates/keel-app/src/settings.rs:704-709` ("Folder sizes: In local folders"); `sizes.rs:171-176` | None. |

## Planner facts

**How the Sizes view is built.**
- Crate: `keel-app`. `ViewMode::Sizes` (`crates/keel-app/src/pane.rs:24-26`) is not a widget of its own. `pane.rs:222` sets `tab.sizes_view`, and `pane.rs:246` sends Sizes to the same `view_details::ui` as Details.
- `view_details.rs` reuses the `egui_extras::TableBuilder` virtual-row table. In Sizes mode:
  - The Ext column becomes "Share", drawn by `share_bar` (`view_details.rs:468-478`). It uses `allocate_exact_size` and `painter().rect_filled`, with the accent fill on a muted track.
  - Two extra columns, "Files" and "Of used", are added (`120-124`, `184-193`, `351-377`).
  - The headers can't be clicked; the view always sorts biggest first (`148-156`).
  - Row ordering (folders mixed with files, unknown sizes last) is in `Tab` (`crates/keel-app/src/tab.rs:129-135,302,631`), fed by `sizes::sync_tab` (`sizes.rs:132-149`).
- Data source, through `sizes::size_of` (`crates/keel-app/src/sizes.rs:108-120`):
  - If the folder is in a library source, the total comes from `LibraryUi::folder_total` (`crates/keel-app/src/library.rs:2957`). That is filled asynchronously by `ask_sizes` (`library.rs:2967-2980`), which calls `LibraryBackend::folder_sizes` (`crates/keel-app/src/backend.rs:1136-1185`; `dirs_only`, `SIZES_MAX = 100_000`, `backend.rs:35`). In-process this goes to `keel_core::Library::folder_sizes` (`crates/keel-core/src/library.rs:1285-1302`); attached to the daemon it calls the `library.folder_sizes` RPC.
  - Otherwise the total comes from the session `Sizes` cache (`sizes.rs:25-106`). It is keyed by path plus the folder's mtime and capped at 500k entries.
- "Of used" divides by the drive's used space (total minus free) from `sidebar::drive_in` (`view_details.rs:67`; the `Drive` tuple is in `sidebar.rs:10-11`).
- The view only shows one level. Navigation uses the normal Enter/Backspace.

**How folder totals are computed and stored.**
- In the library (keel-core):
  - Table `folder_total(record PK, bytes, files)` plus the dirty queue `folder_dirty(record PK)`, with triggers on `record` insert/delete/update of parent, kind, size, flags (`crates/keel-core/src/schema.sql:317-343`).
  - `totals::refresh` (`crates/keel-core/src/totals.rs:52-74`) runs after each watcher batch or full walk. With more than 256 dirty folders that are also over 25% of all folders, it does a full `rebuild` (`78-120`: SUM(size) GROUP BY parent, then rolls up deepest first). Otherwise it does an incremental `update` (`124-176`: a heap by depth, recomputing from direct children and walking up while the total changes).
  - `totals::folder_sizes` (`181-247`) returns the folder's own total plus its children sorted by size (`ORDER BY s IS NULL, s DESC`, with LIMIT `max`+1 for `truncated`).
  - Cost is under 10% of a first walk (`docs/performance.md:15`).
- Outside the library: `keel_vfs::sizes::folder_total` (`crates/keel-vfs/src/sizes.rs:26-82`) is a serial recursive walk through the VFS `Provider`. It's depth-capped at 512 and checks the cancel flag on every entry. It reports `(path, mtime, Walked{bytes, files, unreadable})` for every folder below through the `seen` callback, so descending later is instant.
- Semantics, the same in both paths:
  - Apparent size only (`Entry.size`; `record.size`). No allocated or compressed size is gathered anywhere.
  - Links aren't counted or followed (`flags & LINK`, `index.rs:56`; `e.is_link`).
  - A hard link counts once per path. `record.fs_id` (volume serial + file id, or dev + inode) is available to dedupe but isn't used.
  - Cloud placeholders count by their stated size.
  - Library totals leave out ignored files, and hidden files when the source excludes them.
- Each `Total` is `{bytes, files}`, with no folder or item count (`totals.rs:14-19`).

**Scan/walk job API.**
- `Jobs::folder_sizes(dir, folders: Vec<(VPath, Option<SystemTime>)>, router, tx) -> job id` (`crates/keel-app/src/jobs.rs:284-335`):
  - Walks each subfolder in turn and sends `Msg::FolderSizes(Vec<(VPath, Option<SystemTime>, Total)>)` after each one, including partial results after a cancel.
  - Progress is `Progress{total_items = #folders, done_items, current = folder name}`.
  - Ends with an error if any folder was unreadable ("N folders could not be read").
- It's started by `AppState::calculate_sizes(pane, fresh)` (`crates/keel-app/src/state.rs:1214-1232`). It starts on its own after a listing if the setting is on (`state.rs:1279-1286`), or from the Sizes view (`view_details.rs:438-443`, and the button at `87-96`).
- The generic `Jobs::spawn(title, tx, work: FnOnce(&dyn Fn(Progress), &AtomicBool) -> Result<()>)` (`jobs.rs:434-490`) runs `work` on a named worker thread. It throttles progress to 50 ms, catches panics, and finishes with `Msg::JobDone`.
- CLI and API:
  - `keel du <path> [--json]` (`crates/keel-app/src/cli.rs:60-62`; `commands.rs:325-445`) uses `library.folder_sizes`. On NOT_FOUND for a local path it falls back to `walk_sizes`.
  - The `library.folder_sizes` API (`crates/keel-api/src/ops.rs:121-122,1317-1323`; types at `crates/keel-api/src/types.rs:1236-1262`) takes `{path, max?=1000, dirs_only?}` and returns `{path, bytes?, files?, children[{name, path, is_dir, bytes?, files?}], truncated}`. It's read-only.

**What the jobs panel offers.**
- Local jobs (`jobs.rs:529-598`): title, a progress bar with percentage (`desired_width 180`), the current item text, and Cancel while running. A finished job clears after 5 s (`CLEAR_AFTER`, `jobs.rs:19`). A failed job, or one that skipped items, stays with a × Dismiss button. There's a "Cancelling…" state, and the area scrolls up to 150 px.
- Library jobs (`crates/keel-app/src/library_ui.rs:652-690`): same layout, plus Pause/Resume for the hash job, and states Queued/Running/Paused/Done/Failed/Cancelled.
- Neither kind shows elapsed time or throughput, and there's no per-job details view.

**egui painting approach for a treemap.**
- egui/eframe 0.31 on wgpu (`crates/keel-app/Cargo.toml:27-29`). The existing custom drawing is immediate-mode `ui.painter()` primitives:
  - `rect_filled` / `rect_stroke` / `text` / `galley` / `image` (`view_details.rs:469-478`, `view_media.rs:256-421`, `media_viewer.rs:479-701`).
  - One `Shape::convex_polygon` (`view_media.rs:421`).
  - Textures via `ctx.load_texture` from `ColorImage` (`view_grid.rs:117-119`, `media_viewer.rs:377`).
- For a treemap: compute a squarified layout off-thread from `folder_sizes` data.
  - Flat-shaded tiles: allocate one rect with `ui.allocate_rect(rect, Sense::click())` and paint the tiles with `painter().rect_filled`. Batching them into a single `egui::Mesh` (`Shape::mesh`) avoids per-shape overhead with thousands of tiles.
  - Cushion shading: the WinDirStat-style per-pixel lighting is best rasterised on a worker into a `ColorImage` and uploaded once with `load_texture`, then hit-tested by mapping the pointer position back to the layout rects.
- No mesh or texture-based chart code exists yet. A new `ViewMode::Treemap` arm would go in `pane.rs:243-249`, next to the view buttons at `pane.rs:357-370`.

**Where per-extension stats could come from.**
- Library store (one SQLite per source, `docs/library.md:15`):
  - `record(kind, size, name, path, parent, flags, cas_id, fs_id)` has no extension column. An extension must be derived from `name` in SQL (e.g. `lower(substr(name, instr…))`, or a Rust-side group).
  - `search.rs:625` filters by extension with `r.name LIKE ? ESCAPE '\'`.
  - `record_size` (`schema.sql:172`, partial index `size WHERE kind = 0`) serves top-N largest files directly: `SELECT … FROM record WHERE kind=0 AND flags&4=0 ORDER BY size DESC LIMIT N`.
  - For a subtree, filter on `path LIKE 'rel/%'`, or use a recursive CTE as in `inspect.rs:266-276`.
  - Kind→extension lists exist in `KindFilter::extensions` (`crates/keel-core/src/search.rs:70`), useful for colour groups.
- Existing aggregates: the `counts` row (records, files, bytes; `schema.sql:113-130`) and `Library::stats` (`library.rs:1177-1250`) give per-source totals only. `chips::top_extensions` (`chips.rs:360-362`) counts extensions over file names from `files.names` (`library.rs:1181-1192`), not bytes.
- Outside the library: extend `keel_vfs::sizes::walk` (`vfs/sizes.rs:40-82`) to accumulate a `HashMap<ext, (bytes, files)>` from `Entry.ext` (already lowercase, `entry.rs:23-24`).

**Existing "largest files" and duplicate UI.**
- Largest:
  - No dedicated UI.
  - The Sizes view sorts one folder's children biggest first.
  - The Inspector's Folder section shows the "Biggest" child, the folder count, size and files (`crates/keel-app/src/inspector.rs:271-285`; `crates/keel-core/src/inspect.rs:318-330`).
  - `keel du` lists children biggest first.
  - The library search `size:` filter and Size chips (`docs/library.md:39,57`).
- Duplicates:
  - The Duplicate finder window (`library_ui.rs:999-1060`) offers collapsible groups "n × size — name", "Keep this one" (builds a delete plan through the preview, `keep_one`, `crates/keel-app/src/library.rs:447`), "Open folder", source and offline badges, and a reclaimable total. It shows at most 500 groups (`library.rs:47-48`).
  - Also: the Overview duplicate summary (`library_ui.rs:358`), the Copies column and Inspector Copies section (`docs/files.md:44`), and the `duplicates` API (`crates/keel-api/src/ops.rs:110,1079`).
  - Backed by `Library::duplicates(min_size)` (`crates/keel-core/src/hash.rs:1117-1170`): BLAKE3 `cas_id` across sources, hard links deduped via `fs_id`, drifted files excluded.
