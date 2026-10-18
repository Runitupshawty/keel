# File properties

Everything's property search functions (`artist:`, `width:`, `sha256:`, `shortcut-target:` ...) ask about a file's content, not what the index holds. The `keel-props` crate reads them. It is a library with no UI: the query language (`keel-query`) and property indexing build on it, and they are what you type into. This page says which properties exist, where each one comes from and what limits apply.

## Where values come from

Each property comes from one of two places:

- **Keel's own readers**, on every platform. They read only the bytes they need and never trust a size the file declares:
  - **File signature**: the file's type from its first 4 KiB (magic numbers), not its name. Images, audio, video, documents, archives, executables and fonts, and zip-based formats told apart by their first entries (`docx`, `xlsx`, `pptx`, `odt`, `epub`, `jar`, `apk`). This gives `file-signature:` (a short id such as `png`), `content-type:` (`image/png`) and `perceived-type:` (`image`, `audio`, `video`, `text`, `compressed`, `document`, `system`, `application`, `font`). A file with no signature that reads as text is `text/plain` and `text`.
  - **Shortcuts**: a symbolic link's target on every platform, and a Windows shell link's (`.lnk`) target: LinkInfo's local path or network share plus its suffix, else the environment-variable target (`%windir%\notepad.exe`), else the relative path. `.lnk` files are read on any platform. An Internet shortcut's (`.url`) address is `url:`.
  - **PE version resources** (`.exe`, `.dll` and other PE images): FileVersion, ProductVersion, ProductName, CompanyName, LegalCopyright, LegalTrademarks and FileDescription. Only the headers, the resource directory path to the version resource and the resource itself (at most 64 KiB) are read. `version:` is the numeric file version (`1.2.3.4`); `product-version:` is the ProductVersion string as Explorer shows it.
  - **Text statistics**: `character-encoding:` from the byte order mark, UTF-8 validity and a UTF-16 check (`ASCII`, `UTF-8`, `UTF-8 BOM`, `UTF-16LE`, `UTF-16BE`, `UTF-32LE`, `UTF-32BE`, `ANSI`, or `binary`), `line-count:` (a last line without a newline counts) and `word-count:` (runs of non-space characters). Files over 64 MiB get no counts (unknown); their encoding comes from their first 64 KiB.
  - **Hashes**: `md5:`, `sha1:`, `sha256:`, `crc32:` and `crc64:` (`crc:`), as lowercase hex. CRC-32 is the zip one; CRC-64 is CRC-64/XZ (as 7-Zip prints it). The file is read once for every hash asked for together, and a read can be cancelled between 256 KiB chunks.
  - **Deleted items**: `date-deleted:` of an item in a Windows Recycle Bin (`$Recycle.Bin\<SID>\$R...`) comes from its `$I` record (versions 1 and 2: deletion time, original path, size), read on any platform so a Windows disk mounted elsewhere works too; of an item in a freedesktop.org Trash (`~/.local/share/Trash/files`, `.Trash-<uid>`, `.Trash/<uid>`) from its `.trashinfo` file (`DeletionDate`, local time). An item inside a deleted folder has the folder's date.
  - **Audio tags**: ID3v2 (2.2, 2.3 and 2.4, with unsynchronisation and per-frame flags) and ID3v1 tags; MPEG audio frames for sample rate, channels and bit rate, with Xing, Info and VBRI headers for the length of variable-bit-rate files; FLAC STREAMINFO and Vorbis comments; WAV `fmt `, `data`, `LIST/INFO` and embedded ID3 chunks. ID3v2 wins over ID3v1. Numeric ID3 genres (`(17)`) become names (`Rock`). `audio-bitrate:` is in kbit/s.
- **The Windows Property System**, on Windows only: the same `IPropertyStore` Explorer's columns and Details pane use, with every property handler installed (photos, Office documents, videos, mail). It answers every function with a Windows property in the tables below when Keel's own reader has no value. Any canonical property name also works as a function: `System.Photo.FNumber`, `System.Music.Composer`. Durations come back as durations, bit rates in kbit/s and frame rates in frames per second.

A function with no reader on a platform gives **unknown** (Everything's `unknown`), not an error: on Linux and macOS, image, video and document properties are unknown until the readers listed under "Not yet" exist.

## Cloud placeholders

A file whose data lives only in the cloud (OneDrive, iCloud Drive, Dropbox and other cloud-file providers) is never downloaded to read a property. On Windows a placeholder is a file with the recall-on-open, recall-on-data-access or offline attribute; on macOS a dataless file. For such files Keel's readers do not open the file, and the Windows Property System is asked only for properties it already has without opening the file (`GPS_FASTPROPERTIESONLY`). The crate's `Options::allow_recall` turns this off for callers that mean to download.

## Limits

| What | Limit |
|---|---|
| File signature | first 4 KiB |
| `.lnk`, `.url` files | 1 MiB, 64 KiB |
| PE version resource | 96 sections, 4,096 entries per resource directory, 64 KiB resource |
| Text counts | 64 MiB per file (`Options::max_text_bytes`) |
| Hashes | no size limit by default (`Options::max_hash_bytes`); cancelable |
| ID3v2 tag, Vorbis comment block | 4 MiB read (the rest, usually cover art, is skipped) |
| MPEG frame search | first 64 KiB after the tag |
| Recycle Bin `$I` record, `.trashinfo` file | 72 KiB, 256 lines |
| Content search | see [Content search](#content-search) |

Every reader of outside bytes has a fuzz target ([fuzzing](fuzzing.md)): `file_signature`, `lnk`, `pe_version`, `audio_tags`, `content_match`, `recycle_info` and `trash_info`.

## Content search

Everything's `content:` searches inside any file, not only library sources. The crate's `content::Matcher` does it; the query language calls it for the files that pass the cheaper terms first. Each content function reads the file's bytes as one encoding:

| Function | Reads the file as |
|---|---|
| `content:` | by byte order mark (UTF-8, UTF-16LE, UTF-16BE, UTF-32), else the UTF-16 check on the first 64 KiB (text with a NUL in every other byte), else UTF-8, or Windows-1252 when the first 64 KiB are not valid UTF-8. Binary files are read as UTF-8, so text inside programs is found. |
| `utf8content:` | UTF-8; invalid bytes never match |
| `utf16content:` | UTF-16LE |
| `utf16becontent:` | UTF-16BE |
| `ansicontent:` | Windows-1252 (not the system code page) |
| `binary:` | raw bytes, against the pattern's UTF-8 bytes; ASCII letters ignore case unless `case:` |
| `hex:` | raw bytes, against hex byte pairs (`hex:504b0304`, `hex:"50 4b 03 04"`, `hex:0x50,0x4b`); always exact |

The options are Everything's modifiers:

- `case:` matches case (the default ignores it, Unicode-aware); `whole-words:` (`ww:`) wants a word boundary at each end of the pattern that is a letter, digit or `_`.
- `regex:` reads the pattern as a regular expression (the Rust `regex` syntax: linear time, no backreferences; compiled size at most 4 MiB, pattern at most 16 KiB).
- By default the file is one text: a pattern can span lines, and `^` and `$` are the file's start and end. `multi-line:` makes `^` and `$` match at line breaks too. `each-line:` matches each line on its own (a `\r` before the `\n` is not part of the line), so a match never spans lines. `dot-all:` lets `.` match a line break.

The file is read in 1 MiB steps and searched in windows that overlap by the longest possible match (4 bytes per pattern character, or the pattern's bytes for `binary:` and `hex:`) or by 64 KiB for a regular expression, so matches across steps are found, and `^`, `$` and word boundaries only match where the whole file has them. A regular expression match longer than 64 KiB that crosses a step boundary can be missed; with `each-line:` a line longer than 1 MiB is searched in 1 MiB pieces.

**Limits.** Every search is bounded per file and per query: by default 64 MiB and 10 seconds per file, 4 GiB and 60 seconds per query (`content::Limits`). A file whose limit is reached without a match is **unknown**, not a non-match; so are folders, and cloud placeholders, which are never read (their content would be downloaded) unless the caller allows recall. The search can be cancelled between steps.

**Library text.** When a file is in a library source whose text job has read it, the caller passes that text and `content:` searches it instead of the file's bytes, so words in PDFs and Office documents are found (and a cloud placeholder's text, already read, is searched without downloading it). The explicit encodings and `binary:`/`hex:` always read the bytes.

**As a property.** `keel_props::get(path, "content-match:<pattern>")` is yes, no or unknown. The name picks the encoding (`content-match:`, `utf8content-match:`, `utf16content-match:`, `utf16becontent-match:`, `ansicontent-match:`), then any of the modifiers `case:`, `no-case:`, `whole-words:` (`ww:`), `regex:`, `each-line:`, `multi-line:`, `dot-all:`, `binary:` and `hex:` come before the pattern: `content-match:ww:invoice`, `utf16content-match:case:regex:^Total`, `content-match:hex:25 50 44 46`. A pattern in double quotes is taken as is (`content-match:"case:x"`). A pattern that cannot be searched for (empty, bad hex, a regular expression that does not compile) is an error. `FileProps::with_text` passes the library's text; `Options::content` shares one query's limits and totals between files.

## Sound-alike names

`soundex:` matches words by American Soundex, the U.S. National Archives' rules (`keel_props::soundex`): every word of the pattern must sound like some word of the name, so `soundex:rupert` finds `Robert smith.txt` and `soundex:ashcraft` finds `Ashcroft.jpg`. Words are runs of Latin letters (diacritics count as their base letter); digits and punctuation separate them.

`pinyin:` (`pinyin-jian:`, `pinyin-quan:`), which matches Chinese names by their pinyin, is not supported: Keel's dependencies have no pinyin table, and adding one is left for later.

## Functions

Function names are Everything's (audit Part 1), with their aliases in parentheses. "no" means no reader of that kind. These tables are generated from the crate's function table; a test fails when they differ.

### File info

| Function | Keel's own reader (every platform) | Windows Property System |
|---|---|---|
| `perceived-type:` | file signature, text check | `System.PerceivedType` |
| `content-type:` | file signature | `System.ContentType` |
| `document-content-type:` | no | no |
| `file-signature:` | file signature | no |
| `shortcut-target:` | symlink, `.lnk` parser | `System.Link.TargetParsingPath` |
| `volume-label:` | no | no |
| `location:` | no | no |
| `computer:` | no | `System.ComputerName` |
| `url:` | `.url` parser | `System.Link.TargetUrl` |
| `owner:` | no | `System.FileOwner` |
| `protected:` | no | no |
| `offline-availability:` | no | `System.OfflineAvailability` |
| `offline-status:` | no | `System.OfflineStatus` |
| `run-count:` | no | no |
| `is-running:` | no | no |
| `shared-with:` | no | `System.SharedWith` |
| `status:` | no | `System.Status` |
| `content-status:` | no | `System.ContentStatus` |
| `character-encoding:` | text stats | no |
| `line-count:` | text stats | no |
| `word-count:` | text stats | no |
| `page-count:` | no | `System.Document.PageCount` |
| `slide-count:` | no | `System.Presentation.SlideCount` |
| `total-editing-time:` | no | `System.Document.TotalEditingTime` |
| `md5:` | hash (streamed) | no |
| `sha1:` | hash (streamed) | no |
| `sha256:` | hash (streamed) | no |
| `crc32:` | hash (streamed) | no |
| `crc64:` (`crc:`) | hash (streamed) | no |
| `digital-signature-name:` | `WinVerifyTrust` (Windows only): embedded or catalog signature | no |
| `digital-signature-timestamp:` | `WinVerifyTrust` (Windows only): embedded or catalog signature | no |
| `version:` | PE version resource | `System.FileVersion` |
| `product-name:` | PE version resource | `System.Software.ProductName` |
| `product-version:` | PE version resource | `System.Software.ProductVersion` |
| `company:` | PE version resource | `System.Company` |
| `copyright:` | PE version resource | `System.Copyright` |
| `trademarks:` | PE version resource | `System.Trademarks` |
| `description:` | PE version resource | `System.FileDescription` |
| `details:` | no | no |

### Dates

| Function | Keel's own reader (every platform) | Windows Property System |
|---|---|---|
| `date-run:` | no | no |
| `date-deleted:` | Recycle Bin `$I` record, Trash `.trashinfo` | `System.Recycle.DateDeleted` |
| `date-saved:` | no | `System.Document.DateSaved` |
| `date-printed:` | no | `System.Document.DatePrinted` |
| `date-received:` | no | `System.Message.DateReceived` |
| `date-sent:` | no | `System.Message.DateSent` |
| `date-released:` | no | `System.Media.DateReleased` |
| `date-acquired:` | no | `System.DateAcquired` |
| `date-changed:` | no | no |
| `date-content-created:` | no | `System.Document.DateCreated` |
| `date-taken:` | no | `System.Photo.DateTaken` |
| `date-encoded:` (`date-media-created:`) | no | `System.Media.DateEncoded` |

### Image

| Function | Keel's own reader (every platform) | Windows Property System |
|---|---|---|
| `width:` (`wide:`) | no | `System.Image.HorizontalSize` |
| `height:` (`high:`) | no | `System.Image.VerticalSize` |
| `dimensions:` | no | `System.Image.Dimensions` |
| `orientation:` | no | `System.Photo.Orientation` |
| `bit-depth:` (`bpp:`) | no | `System.Image.BitDepth` |
| `horizontal-resolution:` | no | `System.Image.HorizontalResolution` |
| `vertical-resolution:` | no | `System.Image.VerticalResolution` |
| `aspect-ratio:` | no | no |
| `camera-maker:` | no | `System.Photo.CameraManufacturer` |
| `camera-model:` | no | `System.Photo.CameraModel` |
| `lens-model:` | no | `System.Photo.LensModel` |
| `aperture:` | no | `System.Photo.Aperture` |
| `f-stop:` | no | `System.Photo.FNumber` |
| `max-aperture:` | no | `System.Photo.MaxAperture` |
| `exposure-time:` | no | `System.Photo.ExposureTime` |
| `exposure-bias:` | no | `System.Photo.ExposureBias` |
| `exposure-program:` | no | `System.Photo.ExposureProgram` |
| `exif-version:` | no | `System.Photo.EXIFVersion` |
| `flash-mode:` | no | `System.Photo.Flash` |
| `focal-length:` | no | `System.Photo.FocalLength` |
| `35mm-focal-length:` | no | `System.Photo.FocalLengthInFilm` |
| `iso-speed:` | no | `System.Photo.ISOSpeed` |
| `light-source:` | no | `System.Photo.LightSource` |
| `metering-mode:` | no | `System.Photo.MeteringMode` |
| `brightness:` | no | `System.Photo.Brightness` |
| `contrast:` | no | `System.Photo.Contrast` |
| `saturation:` | no | `System.Photo.Saturation` |
| `white-balance:` | no | `System.Photo.WhiteBalance` |
| `subject-distance:` | no | `System.Photo.SubjectDistance` |
| `latitude:` | no | `System.GPS.LatitudeDecimal` |
| `longitude:` | no | `System.GPS.LongitudeDecimal` |
| `altitude:` | no | `System.GPS.Altitude` |

### Audio, video and music

| Function | Keel's own reader (every platform) | Windows Property System |
|---|---|---|
| `album:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.AlbumTitle` |
| `album-artist:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.AlbumArtist` |
| `artist:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.Artist` |
| `authors:` | no | `System.Author` |
| `composer:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.Composer` |
| `conductor:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.Conductor` |
| `director:` | no | `System.Video.Director` |
| `producer:` | no | `System.Media.Producer` |
| `writer:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Media.Writer` |
| `genre:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.Genre` |
| `mood:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.Mood` |
| `title:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Title` |
| `subtitle:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Media.SubTitle` |
| `track:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.TrackNumber` |
| `year:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Media.Year` |
| `beats-per-minute:` (`bpm:`) | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.BeatsPerMinute` |
| `initial-key:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.InitialKey` |
| `length:` (`duration:`) | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Media.Duration` |
| `audio-bitrate:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Audio.EncodingBitrate` |
| `audio-bits-per-sample:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Audio.SampleSize` |
| `audio-channels:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Audio.ChannelCount` |
| `audio-format:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Audio.Format` |
| `audio-sample-rate:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Audio.SampleRate` |
| `audio-track-count:` | no | no |
| `video-bitrate:` | no | `System.Video.EncodingBitrate` |
| `video-format:` | no | `System.Video.Compression` |
| `video-track-count:` | no | no |
| `subtitle-track-count:` | no | no |
| `total-bitrate:` | no | `System.Video.TotalBitrate` |
| `frame-count:` | no | no |
| `frame-rate:` (`fps:`) | no | `System.Video.FrameRate` |
| `language:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Language` |
| `part-of-a-compilation:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.IsCompilation` |
| `part-of-set:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Music.PartOfSet` |
| `period:` | no | `System.Music.Period` |
| `encoded-by:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Media.EncodedBy` |
| `publisher:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Media.Publisher` |
| `parental-rating:` | no | `System.ParentalRating` |
| `parental-rating-reason:` | no | `System.ParentalRatingReason` |

### Document and other

| Function | Keel's own reader (every platform) | Windows Property System |
|---|---|---|
| `subject:` | no | `System.Subject` |
| `comment:` | ID3v2/ID3v1, MPEG, FLAC, WAV | `System.Comment` |
| `category:` | no | `System.Category` |
| `tags:` (`tag:`) | no | `System.Keywords` |
| `rating:` | no | `System.Rating` |
| `people:` | no | `System.Photo.PeopleNames` |
| `last-author:` | no | `System.Document.LastAuthor` |
| `software:` | no | `System.ApplicationName` |
| `from:` | no | `System.Message.FromAddress` |
| `to:` | no | `System.Message.ToAddress` |

The other Everything functions (name, path, size, the file system dates, attributes, children and descendants, duplicates) are answered by the index and the query language, not by a property reader; `content:` and its forms by [content search](#content-search).

## Not yet

- Keel's own readers for image (EXIF, dimensions), video and Office document properties on Linux and macOS (the Windows Property System covers them on Windows); macOS `MDItem` attributes.
- `volume-label:`, `location:`, `protected:`, `aspect-ratio:`, track counts and `frame-count:`, `document-content-type:` and `details:`.
- Content indexing (keeping file content in the index per folder, Everything 1.5), `pinyin:` matching, ANSI content in a system code page other than Windows-1252, and Windows XP's `INFO2` Recycle Bin records.
- `is-running:`, and Ogg/Opus and MP4 audio tags. Run counts (`run-count:`, `date-run:`) come from Keel's run history and indexed properties from the property index ([Search: property index](files.md#property-index)), not from this crate.
- ANSI strings in `.lnk` files, ID3v1 tags and WAV INFO chunks are read as Windows-1252, not the system's code page. Windows list values (several artists, keywords) are split at `; `, so a value that itself holds `; ` splits in two.
