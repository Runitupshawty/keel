# The search language

Keel reads voidtools Everything's search syntax: operators, groups, wildcards, regular
expressions, path terms, ranges, lists, modifiers, search functions, date and size constants,
and filter macros. This page lists what Keel's own engine (the `keel-query` crate) understands
today. Everything still receives the text unchanged when it is Keel's search backend, so
everything Everything supports keeps working there.

> **Where it works.** This language is the one query language of Keel's own searches: the
> search tab and **Search here** box of every pane, `keel search`, the API's `system.search`
> and `search`, and MCP use it. The table says how each backend runs it.

| Backend | How the language runs |
|---|---|
| Keel's own index (the NTFS index, the home-folder and folder/volume name indexes, a file list, the plain walk of a folder no index holds) | Whole: the index scans its names for the query's longest plain word and the compiled query decides each candidate, with the entry's path, size, dates, attributes and folder counts. Parse errors come back with the place in the text. |
| Everything | Everything receives the text as typed and runs its own language (which has more functions than Keel's engine); the search tab shows Keel's check of the text beside the box. |
| Spotlight, `locate` | Whole, but evaluated by Keel: the backend is asked for the names that contain the query's longest plain word (all names when it has none), up to 10,000, and Keel checks those. A query with no plain word (`ext:pdf`) therefore reads at most the first 10,000 names the backend gives. |
| The library | OR, `!`, groups, wildcards, `regex:`, path terms and the other functions work; the library's own terms (`tag:`, `content:`, `camera:`, `kind:`, words that also match document text) keep their meaning. See [Library](library.md#searching). |

What an index knows about an entry is what the language can ask: the NTFS index has size
(a folder's is the total below it), size on disk, created, modified, accessed and recently
changed dates, attributes, file ids, hard link and stream counts, and child and descendant
counts from its folder totals; the home-folder and folder/volume name indexes have size
(files), modified and created dates, attributes and child counts; a file list has what its
columns hold; Spotlight and `locate` hits have size and modified date. A property a source does
not have is `unknown`: it fails every comparison and matches `unknown`. Tree functions work on
the name indexes and the NTFS index (with a table of parents built when a query first uses
one); set functions (`dupe:`, `unique:`, `count:`) see the whole result of one index, so on a
machine with several NTFS volumes each volume is deduplicated on its own.

## Terms and operators

| Syntax | Meaning |
|---|---|
| `a b` | both (space is AND) |
| `a\|b` | either (OR); spaces around `\|` are ignored |
| `!a` | not |
| `<a b>\|c` | grouping |
| `"a b"` | quotes: spaces, `\|`, `!`, `<`, `>` and `;` inside are text |
| `*`, `?` | wildcards: any run of characters, one character; neither crosses `\`. `**` crosses `\` |
| `C:\work`, `\work` | a term with `\` matches the full path instead of the name |
| `work\` | a trailing `\` names a folder: the items inside folders whose name ends in `work` |
| `regex:^a.*z$` | a regular expression; one with an escaped `\\` matches the full path. On macOS and Linux a `/` in a regex is a path separator too (`regex:/docs/[^/]+\.pdf$`) |
| `function:value` | a search function (below) |
| `function*:text` | the rest of the query, as typed, is the function's text (`name*:a b\|c`) |

**Precedence.** OR binds before AND, like Everything: `a b|c` is `a <b|c>`. The option
**Operator precedence** (AND first) or the modifier `and-precedence:` anywhere in the query
makes it `<a b>|c`; `or-precedence:` switches back. `!` binds tightest: `!a|b` is `<!a>|b`.

**Wildcards** match the whole name (`*.txt` does not match `a.txt.bak`) while **Match whole
filename when using wildcards** is on (the default); off, they match anywhere in the name. In a
path term they match the whole path (`C:\work\**.txt`).

**Options** (Everything's Search menu and Options → Search): Match Case, Match Diacritics, Match
Whole Words, Match Path, Match Prefix, Match Suffix, Ignore Punctuation, Ignore White-space,
Regular Expressions, Match whole filename when using wildcards, **Allow literal operators**
(`AND`, `OR` and `NOT` in capitals are operators), **Replace forward slashes with
backslashes** (`work/` is a path term), operator precedence, and the size unit (1,024 or
1,000 bytes). Modifiers override them per term.

**Errors** carry the range of the query they are about, so the search field can mark it: an
unclosed `<`, an unknown `name:` (quote the text to search for it: `"foo:bar"`), a value that
does not parse (`size:big`, `dm:soon`), a regular expression that does not compile or is too
large (over 1 MiB compiled), and a function that is not supported yet. Groups nest at most 32
deep and macros 8 deep.

## Function values

| Syntax | Meaning |
|---|---|
| `size:1mb` | equal within the value's granularity: `1kb` is 1,024..2,047 bytes, `2024` the whole year |
| `size:>1mb`, `>=`, `<`, `<=` | compared with the value (`>1mb` is over 1,048,576 bytes); dates compare with the whole period (`dm:>2024` is after 2024) |
| `=`, `==` | equal (for text: the whole value) |
| `!=`, `!` | not equal (for text: does not contain) |
| `size:1mb..10mb`, `size:1mb-10mb` | a range, both ends included; `..10mb` and `1mb..` are open |
| `ext:jpg;png` | a list: any of the values (quote a `;` that is text) |
| `size:unknown` | the property is not known (not indexed or not readable); `!unknown` is known |
| `run-count:` | no value: any non-zero number or non-empty text |
| `stem:$parent-name:` | `$property:` is the item's own value (`no-substitute:` turns it off) |
| `regex:^(.)  $1:` | `$0:`..`$9:` are the last regular expression's captures; a query that uses them keeps its terms in order |

Character macros put a character into a term: `quot:` (`"`), `apos:` (`'`), `amp:` (`&`), `lt:`
(`<`), `gt:` (`>`), `#65:` (by code, decimal) and `#x41:` (hex).

## Modifiers

Modifiers prefix a term or a function and combine: `case:regex:^Read`, `nopath:ww:report`,
`metric:size:>1.5gb`, `file:report`. Global modifiers change the whole query wherever they are
typed.

| Modifier | Meaning |
|---|---|
| `all-strings:` | every `;`-separated value of the property must match |
| `and-filter:` | global: combine the selected filters with AND |
| `and-precedence:` | global: AND binds before OR |
| `binary:` | compare bytes exactly (no case or accent folding) |
| `case:` | match case |
| `no-case:`, `nocase:` | ignore case (the default) |
| `diacritics:`, `diac:` | match accents (é is not e) |
| `no-diacritics:`, `nodiacritics:`, `no-diac:`, `nodiac:` | ignore accents (the default) |
| `dot-all:` | regex: `.` also matches a line break |
| `each-line:` | match each line of the value on its own |
| `end-with:`, `endwith:`, `$:` | the name (or value) ends with the text |
| `no-fast-regex:` | accepted; Keel's regex engine has no separate fast path |
| `file:`, `files:` | files only |
| `folder:`, `folders:` | folders only |
| `global:` | accepted; affects highlighting of every regex match (Phase 16) |
| `hex:` | the text is two-digit hex codes (`hex:4b65656c`) |
| `hex-to-number:` | read the name or value as a hex number and compare numerically |
| `no-highlight:` | accepted; the term is not highlighted (Phase 16) |
| `from-disk:`, `fromdisk:` | accepted; read the property from disk rather than the index (the caller decides) |
| `len:` | compare the length of the name (or value) in UTF-16 units: `len:>20`, `len:stem:4` |
| `menu-break:` | global: start a new filter-menu column (Phase 16) |
| `menu-separator:` | global: show the filter as a menu separator (Phase 16) |
| `metric:` | 1 KB = 1,000 bytes |
| `no-metric:`, `nometric:` | 1 KB = 1,024 bytes (the default) |
| `multi-line:`, `multiline:` | regex: `^` and `$` match at every line |
| `multi-string:`, `multistring:` | split the value on `;` and match any part |
| `no-multi-string:`, `nomultistring:` | match the value as one string |
| `no-!:` | `!` in this term is text |
| `or-filter:` | global: combine the selected filters with OR |
| `or-precedence:` | global: OR binds before AND (the default) |
| `path:`, `\:` | match the full path |
| `no-path:`, `nopath:` | match the name only |
| `pinyin:`, `py:` | not yet supported: needs a Pinyin table |
| `no-pinyin:`, `nopinyin:` | no Pinyin matching (the default) |
| `pinyin-jian:`, `pyj:` | not yet supported: Chinese characters by Pinyin initials |
| `pinyin-quan:`, `pyq:` | not yet supported: Chinese characters by full Pinyin |
| `prefix:` | the text starts a word |
| `no-prefix:`, `noprefix:` | match anywhere (the default) |
| `ignore-punctuation:`, `nopunc:` | ignore punctuation on both sides |
| `no-ignore-punctuation:` | punctuation counts (the default) |
| `regex:` | the text is a regular expression |
| `no-regex:`, `noregex:` | the text is not a regular expression |
| `enable-result-omissions:` | global: result omissions on |
| `disable-result-omissions:` | global: result omissions off |
| `no-reorder:` | global: evaluate terms in the order typed |
| `search-current-results:` | global: search within the current results |
| `no-search-current-results:` | global: always search everything |
| `soundex:` | the name sounds like the text (American Soundex of the name without its extension) |
| `star-all:` | `*` also matches `\` |
| `start-with:`, `startwith:`, `^:` | the name (or value) starts with the text |
| `no-substitute:` | `$property:` is text |
| `suffix:` | the text ends a word |
| `no-suffix:`, `nosuffix:` | match anywhere (the default) |
| `enable-temp-result-omissions:` | global: temporary result omissions on |
| `disable-temp-result-omissions:` | global: temporary result omissions off |
| `to-integer:` | read the name (or value) as an integer and compare numerically: `to-integer:>100` |
| `to-number:`, `#:` | read the name (or value) as a number and compare numerically |
| `to-string:` | compare a number property as text: `to-string:size:5000` |
| `ungreedy:` | regex: quantifiers are lazy |
| `whole:`, `whole-filename:`, `wholefilename:`, `wfn:`, `entire:` | the whole name (or value) equals the text |
| `no-whole:`, `no-whole-filename:`, `nowholefilename:`, `nowfn:` | match part of the name (the default) |
| `ignore-whitespace:`, `nows:` | ignore spaces on both sides |
| `no-ignore-whitespace:` | spaces count (the default) |
| `whole-words:`, `ww:`, `wholeword:`, `wholewords:`, `whole-word:` | match whole words |
| `no-whole-words:`, `noww:`, `no-ww:`, `nowholeword:`, `nowholewords:` | match inside words (the default) |
| `wildcards:` | `*` and `?` are wildcards (the default) |
| `no-wildcards:`, `nowildcards:` | `*` and `?` are text |

Accent folding covers Latin-1 and Latin Extended-A letters and combining marks; other scripts
compare as typed (case still folds).

## Search functions

### Name, path and type

| Function | Matches |
|---|---|
| `name:` | the name contains the text (wildcards: the whole name) |
| `stem:` | the name without its extension (folders: the whole name) |
| `extension:` | the extension contains the text |
| `ext:` | the whole extension, `;` lists (`ext:jpg;png`), leading `.` ignored; `ext:` alone: has an extension |
| `full-path:`, `fullpath:` | the full path contains the text |
| `full-path-length:`, `fullpathlen:` | length of the full path in UTF-16 units |
| `name-length:`, `namelen:` | length of the name in UTF-16 units |
| `parent:`, `infolder:`, `nosubfolders:` | the exact parent folder (no subfolders); a trailing `\` is ignored |
| `parent-name:`, `parentname:` | the parent folder's name contains the text |
| `parent-count:`, `depth:`, `parents:` | the number of folders above the item (`C:\a\b.txt` is 2) |
| `parent-file-id:`, `parentfrn:` | the parent folder's file id (when the source has one) |
| `file-id:`, `frn:` | the file id (NTFS file reference number, inode) |
| `root:` | items with no parent folder (drives, `/`) |
| `type:` | the file type description (`Text Document`, `File folder`); Keel's table by extension, or the platform's when the source provides it |
| `kind:` | Keel: `folder`, `file`, `audio`, `video`, `image`, `document`, `executable`, `archive` |
| `in:` | Keel: the item is inside this folder, at any depth |
| `ancestor:` | a folder above the item has this full path (`ancestor:C:\work` is anything inside it; wildcards: `ancestor:**\photos`, where `**` crosses folders) |
| `ancestor-name:` | a folder above the item has a name containing the text |
| `pathlist:` | the full path is one of a `;` list (`pathlist:C:\a.txt;C:\b.txt`); any case |
| `system-index:`, `si:` | the Windows Search index finds the item for these words (`si:budget`): in its text, properties or name; the hits are merged into the results and checked against the rest of the query (Windows only, [Windows Search](files.md#windows-search-si)) |
| `filelist:` | the name is one of a `\|` or `;` list (`filelist:"a.txt\|b.txt"`, quoted so `\|` is not OR); whole names, any case unless `case:` |
| `filelist-filename:` | the name of the file list the item comes from (a file list tab's, or a list in Settings → Search index → File lists) contains the text (`filelist-filename:music.efu`); unknown for items not from a list |
| `filelist-full-path:` | the full path of the file list the item comes from contains the text (`filelist-full-path:D:\lists`) |

Paths are compared with `/` read as `\`, so the same terms work on Windows, macOS and Linux
(`\home\me` matches `/home/me`).

### Size, counts and attributes

| Function | Matches |
|---|---|
| `size:` | size in bytes; units and constants (below) |
| `size-on-disk:`, `sizeondisk:` | allocated size in bytes |
| `child-count:`, `childcount:` | files and folders directly inside a folder (`child-count:0` is `empty:`) |
| `child-file-count:`, `childfilecount:` | files directly inside a folder |
| `child-folder-count:`, `childfoldercount:` | folders directly inside a folder |
| `empty:` | folders with no files and no subfolders |
| `attributes:`, `attr:`, `attrib:`, `attribute:` | has all these attributes: letters (`attrib:RHS`) or a hex mask (`attrib:0x20`) |
| `run-count:`, `runcount:` | how many times Keel opened the item ([run history](files.md#run-history)) |
| `descendant-count:` | files and folders anywhere below a folder |
| `descendant-file-count:` | files anywhere below a folder |
| `descendant-folder-count:` | folders anywhere below a folder |
| `hard-link-count:` | how many names (hard links) a file has; `unknown` when the source does not read them |
| `hard-link-filenames:` | any of the file's names (full paths) matches: `hard-link-filenames:**\backup\*` |
| `alternate-data-stream-count:`, `adscount:` | how many alternate data streams (NTFS named streams) a file has |
| `alternate-data-stream-names:`, `adsnames:` | any stream name matches: `adsnames:Zone.Identifier` |
| `container-file-count:` | how many files an archive holds |
| `container-filenames:` | any path inside an archive matches: `container-filenames:*.pdf` |

Child and descendant counts come from the index's folder totals when it keeps them; otherwise
they are counted through the index's parent links, visiting at most 100,000 items per folder
(past that the count is `unknown`). Hard links, streams and archive contents are read only for
items that passed the cheaper terms, and are `unknown` where the source cannot read them.

Attribute letters: `A` archive, `C` compressed, `D` directory, `E` encrypted, `H` hidden, `I` not
content indexed, `L` reparse point, `M` recall on data access, `N` normal, `O` offline, `P` sparse,
`R` read-only, `S` system, `T` temporary, `U` recall on open, `V` integrity stream, `X` no scrub data.

### Tree functions

These look at other items of the index through its parent links. The value is a name, matched
like a plain term (contains the text; with wildcards, the whole name); `;` lists any of
several names.

| Function | Matches |
|---|---|
| `child:` | folders with a file or folder directly inside whose name matches (`child:desktop.ini`); `child:` alone: folders that are not empty |
| `child-file:` | folders with a matching file directly inside |
| `child-folder:` | folders with a matching folder directly inside |
| `descendant:` | folders with a matching file or folder anywhere below (`descendant:*.psd`) |
| `descendant-file:` | folders with a matching file anywhere below |
| `descendant-folder:` | folders with a matching folder anywhere below |
| `sibling:` | items with another item of a matching name in the same folder (`*.jpg sibling:*.xmp`) |
| `sibling-file:` | ... another file |
| `sibling-folder:` | ... another folder |
| `parent-sibling:` | items whose parent folder has a sibling of a matching name |
| `ancestor-sibling:` | items with a folder above them that has a sibling of a matching name |
| `exists:` | a name next to the item exists in the index; `..` goes up and `\` into subfolders (`exists:..\README`), a full path is looked up as is; with substitution: `*.raw !exists:$stem:.jpg` is every raw file without a JPEG of the same name |
| `file-exists:` | like `exists:`, and it is a file |
| `folder-exists:` | like `exists:`, and it is a folder |
| `parentsize:` | the parent folder's size, with the size values below (`parentsize:>1gb`) |
| `parentdm:` | the parent folder's date modified, with the date values below (`parentdm:today`) |

`root:`, `empty:`, `parent:`, `parent-name:`, `parent-count:` and the `child-count:` family are
in the tables above; they need no other items. Every tree function is bounded: each item visits
at most 100,000 other items (children, descendants, siblings); a search that runs past that
treats the item as `unknown` for that term, never as a wrong answer. Without an index (a folder
listing searched as it is), tree functions are `unknown`.

### Dates

| Function | Date |
|---|---|
| `date-modified:`, `dm:`, `datemodified:` | modified |
| `date-created:`, `dc:`, `datecreated:` | created |
| `date-accessed:`, `da:`, `dateaccessed:` | accessed |
| `date-recently-changed:`, `rc:`, `recentchange:`, `recentlychanged:` | the last change Keel's index saw (created, modified, moved or renamed) |
| `date-run:`, `dr:`, `daterun:` | the last time Keel opened the item |
| `date-deleted:` | deleted (Recycle Bin items) |
| `date-saved:`, `date-printed:`, `date-received:`, `date-sent:`, `date-released:`, `date-acquired:`, `date-changed:`, `date-content-created:`, `date-taken:`, `date-encoded:`, `date-media-created:` | read from the file's properties when the source provides them; otherwise `unknown` |

Every date function has component forms: the long name plus `-date`, `-day`, `-day-of-week`
(or `-dow`), `-hour`, `-month`, `-time`, `-year` (`date-created-year:2024`), and the short
forms `dm`, `dc`, `da`, `rc`, `dr` take the suffix without the dash (`dmyear:2024`,
`dcdow:monday`, `rcday:11`):

| Component | Values |
|---|---|
| `-date` | a date, ignoring the time: `dmdate:yesterday`, `dmdate:2024-02-29` |
| `-day` | day of the month, 1..31 |
| `-day-of-week` | `monday`, `mon`, or 0 (Sunday)..6; ranges `monday..friday` |
| `-hour` | 0..23 or `2pm` |
| `-month` | `january`, `jan`, or 1..12 |
| `-time` | a time of day: `10am..2pm`, `17:30`, `17:30:15` |
| `-year` | a year: `2024`, `2000..2010` |

### Date values

Dates are local time (Keel's time zone at the moment of the search). Constants:

| Value | Period |
|---|---|
| `today`, `yesterday`, `tomorrow` | that day |
| `thisweek`, `currentweek`, `thismonth`, `thisyear`, `thishour`, ... | the current calendar period (weeks start on Monday, or Sunday by the option) |
| `lastweek`, `prevweek`, `previousweek`, `lastmonth`, `lastyear`, ... | the previous calendar period |
| `nextweek`, `comingweek`, `nextmonth`, `nextyear`, ... | the next calendar period |
| `pastweek`, `pastmonth`, `pastyear`, `pastday`, `pasthour` | the last week, month, ... up to now |
| `past3days`, `last2weeks`, `prev6months`, `7days`, `12hours`, `30minutes`, `10secs` | the last N units up to now |
| `next2days`, `coming3weeks` | the next N units from now |
| `mtd`, `qtd`, `ytd` | month, quarter, year to date |
| `january`..`december`, `jan`..`dec` | that month this year |
| `sunday`..`saturday`, `sun`..`sat` | the most recent such day (today counts) |
| `unknown` | no date |

Formats: ISO 8601 `2024`, `2024-05`, `2024-05-31`, `2024-05-31T13`, `…T13:05`, `…T13:05:09`,
`…T13:05:09.250` (the period the value names), with `*` for any field (`dm:*-10-*` is any
October; `dm:*-*-*T9:00..*-*-*T17:00` any day between 9:00 and 17:00); ISO basic `20240531` and
`20240531T1305`; `YYMMDD` (`240531`); `YY` (`24`); `MM/YYYY` and `YYYY/MM`; `MM/DD/YYYY`,
`DD/MM/YYYY` or `YY/MM/DD` by the date-order setting, with `/` or `.`; Excel serial days
(`45000`, five digits); FILETIME (nine digits or more, or `0x` hex); a time of day today
(`10am`, `17:30`).

### Size values

Units: `b`, `kb`/`k`, `mb`/`m`, `gb`/`g`, `tb`/`t`, `pb`/`p` (1,024 by default, 1,000 with
`metric:` or the option) and `kib`..`pib` (always 1,024); decimals are allowed (`1.5gb`).
Constants, with Everything's bounds:

| Constant | Size |
|---|---|
| `empty` | 0 |
| `tiny` | 1 byte .. 10 KB |
| `small` | 10 KB + 1 .. 100 KB |
| `medium` | 100 KB + 1 .. 1 MB |
| `large` | 1 MB + 1 .. 16 MB |
| `huge` | 16 MB + 1 .. 128 MB |
| `gigantic` | over 128 MB |
| `unknown` | not known |

## Duplicates and set functions

These work on the whole result, after every other term: `dupe:size ext:jpg` is the JPEG files
that have the same size as another JPEG file. They go anywhere in the query that is ANDed
(also inside a `< >` group), not inside `|` or after `!`. The value of `dupe:`, `distinct:`
and `unique:` is a `;` list of properties, named like their functions: `name`, `stem`, `ext`,
`path` (`full-path`), `parent`, `type`, `size`, `size-on-disk`, `dm`, `dc`, `da`, date
components (`dmyear`, `dcdate`), `attributes`, `child-count`, `run-count` and the other number
and text functions. Text compares by the search's case and accent settings (`case:dupe:name`).
Items whose value is `unknown` are left out.

| Function | Result |
|---|---|
| `dupe:` | items that share the values with another result (`dupe:size;name`); `dupe:` alone compares names |
| `dupe-count:` | only duplicate groups of this many items (`dupe-count:3`, `dupe-count:>=3`) |
| `dupe-min:` | only duplicate groups of at least this many items (2 by default) |
| `dupe-max:` | only duplicate groups of at most this many items |
| `dupe-from:` | only duplicate groups with an item inside one of these folders (`dupe:size dupe-from:D:\incoming`) |
| `distinct:` | the first result for each distinct value (`distinct:ext`) |
| `distinct-sort:` | like `distinct:`, sorted by the value |
| `unique:` | results whose value no other result has |
| `count:` | at most this many results (`count:100`) |
| `sizedupe:` | Everything 1.4: `dupe:size` |
| `namepartdupe:` | Everything 1.4: `dupe:stem` |
| `dmdupe:`, `dcdupe:`, `dadupe:` | Everything 1.4: `dupe:dm`, `dupe:dc`, `dupe:da` |
| `attribdupe:` | Everything 1.4: `dupe:attributes` |

`dupe-count:`, `dupe-min:`, `dupe-max:` and `dupe-from:` without `dupe:` mean `dupe:name`. Only
one of `dupe:`, `distinct:` and `unique:` can be used in a search. Duplicates come grouped and
the groups in order of their value (smallest size first); within a group, and for `unique:` and
`distinct:`, items keep the search's order. Memory is bounded: a duplicate search keeps a small
row and the compared values of each candidate, and stops with "too many results for this
function" past 512 MB instead of growing; narrow it with other terms (`dupe:size size:>1mb`).

## Macros

Filter macros keep files by extension, Everything's default lists; a value after the colon is
ANDed (`audio:abba`):

| Macro | Extensions |
|---|---|
| `audio:` | aac ac3 aif aifc aiff au cda dts fla flac it m1a m2a m3u m4a mid midi mka mod mp2 mp3 mpa ogg opus ra rmi snd spc umx voc wav wma xm |
| `zip:` | 7z ace arj bz2 cab gz gzip jar r00..r29 rar tar tgz xz z zip zst |
| `doc:` | c chm cpp csv cxx doc docm docx dot dotm dotx h hpp htm html hxx ini java lua md mht mhtml odp ods odt pdf potm potx ppam pps ppsm ppsx ppt pptm pptx rtf sldm sldx thmx txt vsd wpd wps wri xlam xls xlsb xlsm xlsx xltm xltx xml |
| `exe:` | bat cmd com exe msi msp ps1 scr |
| `pic:`, `image:` | ani avif bmp gif heic heif ico jfif jpe jpeg jpg pcx png psd svg tga tif tiff webp wmf |
| `video:` | 3g2 3gp 3gp2 3gpp amr amv asf avi bdmv bik d2v divx drc dsa dsm dss dsv evo f4v flc fli flic flv hdmov ifo ivf m1v m2p m2t m2ts m2v m4b m4p m4v mkv mov mp2v mp4 mp4v mpe mpeg mpg mpls mpv2 mpv4 mts ogm ogv pss pva qt ram ratdvd rm rmm rmvb roq rpm smil smk swf tp tpr ts vob vp6 webm wm wmp wmv |

User macros (and, in Phase 16, saved filters and bookmarks) map a name to search text:
`mine:` expands to its text as a group, and a value after it is ANDed. A user macro may replace
a built-in filter macro; it cannot replace a function.

## UI-state functions

These set up the result list (Phase 16) and never filter. The query keeps them for the search
UI: `columns:`, `add-columns:`, `addcol:`, `remove-columns:`, `layout:`, `add-layout:`,
`remove-layout:`, `sort:`, `sort-ascending:`, `sort-descending:`, `sort-mix:`, `no-sort-mix:`,
`folders-first:`, `no-folders-first:`, `folders-first-ascending:`, `folders-first-descending:`,
`alternate-row-color:`, `no-alternate-row-color:`, `on-top:`, `no-on-top:`, `folders-sidebar:`,
`no-folders-sidebar:`, `bookmarks-sidebar:`, `no-bookmarks-sidebar:`, `filters-sidebar:`,
`no-filters-sidebar:`, `preview:`, `no-preview:`, `thumbnails:`, `max-threads:`,
`regular-expression-match0:`..`9:` (`regmatch0:`..`9:`).

## Not yet supported

These Everything functions are recognised and refused with "not yet supported" and the reason,
so a query never silently matches the wrong thing:

| Functions | Why |
|---|---|
| `content:`, `utf8content:`, `ansicontent:`, `utf16content:`, `utf16becontent:`, `textplain:` | content functions (Task 73) |
| `perceived-type:`, `content-type:`, `document-content-type:`, `file-signature:`, `shortcut-target:`, `volume-label:`, `location:`, `loc:`, `computer:`, `url:`, `owner:`, `protected:`, `offline:`, `online:`, `offline-availability:`, `offline-status:`, `is-running:`, `shared-with:`, `status:`, `content-status:`, `character-encoding:`, `line-count:`, `word-count:`, `page-count:`, `slide-count:`, `total-editing-time:`, `md5:`, `sha1:`, `sha256:`, `crc32:`, `crc64:`, `crc:`, `digital-signature-name:`, `digital-signature-timestamp:`, `version:`, `product-name:`, `product-version:`, `company:`, `copyright:`, `trademarks:`, `description:`, `details:` | file properties read from disk (Task 73) |
| `width:`, `wide:`, `height:`, `high:`, `dimensions:`, `orientation:`, `bit-depth:`, `bpp:`, `horizontal-resolution:`, `vertical-resolution:`, `aspect-ratio:`, `camera-maker:`, `camera-model:`, `lens-model:`, `aperture:`, `f-stop:`, `max-aperture:`, `exposure-time:`, `exposure-bias:`, `exposure-program:`, `exif-version:`, `flash-mode:`, `focal-length:`, `35mm-focal-length:`, `iso-speed:`, `light-source:`, `metering-mode:`, `brightness:`, `contrast:`, `saturation:`, `white-balance:`, `subject-distance:`, `latitude:`, `longitude:`, `altitude:` | image properties (Task 73) |
| `album:`, `album-artist:`, `artist:`, `authors:`, `composer:`, `conductor:`, `director:`, `producer:`, `writer:`, `genre:`, `mood:`, `title:`, `subtitle:`, `track:`, `year:`, `beats-per-minute:`, `bpm:`, `initial-key:`, `length:`, `duration:`, `audio-bitrate:`, `audio-bits-per-sample:`, `audio-channels:`, `audio-format:`, `audio-sample-rate:`, `audio-track-count:`, `video-bitrate:`, `video-format:`, `video-track-count:`, `subtitle-track-count:`, `total-bitrate:`, `frame-count:`, `frame-rate:`, `fps:`, `language:`, `part-of-a-compilation:`, `part-of-set:`, `period:`, `encoded-by:`, `publisher:`, `parental-rating:`, `parental-rating-reason:` | audio and video properties (Task 73) |
| `subject:`, `comment:`, `category:`, `tags:`, `tag:`, `rating:`, `people:`, `last-author:`, `software:`, `from:`, `to:` | document properties (Task 73) |
| `network-index-host:` | read by Keel's search tab before the query reaches a backend ([network indexes](devices.md#devices-and-spacedrop)); elsewhere refused |
| `camera:`, `taken:`, `has:`, `contents:`, `source:`, `is:`, `w:`, `h:` | library search terms: they work in the library search only ([Library](library.md#searching)); every other search refuses them |
| `pinyin:`, `pinyin-jian:`, `pinyin-quan:` | Pinyin matching needs a Pinyin table Keel does not ship yet |

## Where Keel differs from Everything

- In Keel's own searches `AND`, `OR` and `NOT` are words (Everything's **Allow literal
  operators** is off, its default): use `|`, `!` and `< >`.
- Keel's own searches turn **Replace forward slashes with backslashes** on, so `docs/x` is a path
  term on every system, and the pane's **Match path**, **Match whole words**, **Match
  diacritics** and **Regular expression** toggles, **Match whole filename when using wildcards**
  (Settings → General → Search) and the API's `prefix`, `suffix`, `ignore_punctuation` and
  `ignore_whitespace` options are the search options above. A wildcard never crosses `\`
  (`*\ann\rep*` with Match path does not reach back through `Users\`; `**` does).
- `in:` (a folder, at any depth) and `kind:` are Keel's own. In Keel's own indexes `in:`
  reads a folder path as typed, also part of the last name: `in:C:\Users\an` keeps `C:\Users\ann`
  and everything below it (and `C:\Users\anna`); a pane's **Search here** folder keeps to whole
  folder names.
- `exists:` looks in Keel's index, not on disk; `dupe-from:` keeps whole duplicate groups that
  have an item in the folder; set functions skip items whose compared value is `unknown`; tree
  functions and duplicate searches are bounded (above) where Everything is not.
- Duplicates by content hash (`dupe:sha256`) wait for the hash functions (Phase 15, Task 73);
  the library's duplicate finder compares content today.
- Size comparisons use the value itself (`size:>1mb` is more than 1,048,576 bytes); equality uses
  the unit's granularity like Everything (`size:1mb` is 1 MB up to 2 MB minus one byte).
- `pastweek` is the last seven days up to now; `lastweek` is the previous calendar week.
- `type:` uses Keel's table by extension unless the item's source reads the platform's type
  description.
- Evaluation order: cheap terms (name, size, indexed dates) run before costly ones (type, dates
  read from file properties, `$property:` terms), unless `no-reorder:` is typed or the query uses
  regex captures.
