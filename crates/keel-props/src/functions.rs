//! Everything's property search functions (audit Part 1: file info, dates, image,
//! audio/video/music and document groups) mapped to Keel's readers and to Windows Property
//! System names.

use crate::hash::HashAlgo;

/// Where a function's value comes from in Keel's own readers (every platform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// No own reader: the Windows Property System only, or not supported yet.
    None,
    /// File signature: short id (`png`).
    SignatureId,
    /// File signature (or text sniffing): MIME type.
    ContentType,
    /// File signature (or text sniffing): Windows perceived type.
    PerceivedType,
    /// Symlink target, or a `.lnk` file's target.
    ShortcutTarget,
    /// An Internet shortcut's (`.url`) `URL=` line.
    Url,
    Pe(PeField),
    Text(TextField),
    Hash(HashAlgo),
    Audio(AudioField),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeField {
    FileVersion,
    ProductVersion,
    ProductName,
    Company,
    Copyright,
    Trademarks,
    Description,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextField {
    Encoding,
    Lines,
    Words,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioField {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Comment,
    Composer,
    Conductor,
    Publisher,
    EncodedBy,
    Subtitle,
    Language,
    Mood,
    InitialKey,
    Writer,
    PartOfSet,
    Track,
    Year,
    Bpm,
    Compilation,
    Length,
    Bitrate,
    SampleRate,
    Channels,
    BitsPerSample,
    Format,
}

/// How a Windows property value becomes the function's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conv {
    /// As the system gives it.
    Same,
    /// Bits per second to kbit/s.
    Kbps,
    /// 100 ns units to a duration.
    Ticks,
    /// Frames per 1,000 seconds to frames per second.
    PerThousand,
    /// `System.PerceivedType`'s number to its name.
    Perceived,
}

/// The function groups of audit Part 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    FileInfo,
    Date,
    Image,
    Media,
    Document,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::FileInfo => "File info",
            Group::Date => "Dates",
            Group::Image => "Image",
            Group::Media => "Audio, video and music",
            Group::Document => "Document and other",
        }
    }
}

/// One Everything function and where Keel reads it.
#[derive(Debug, Clone, Copy)]
pub struct Function {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub group: Group,
    pub reader: Reader,
    /// The Windows Property System canonical name (Windows only).
    pub windows: Option<&'static str>,
    pub conv: Conv,
}

impl Function {
    /// True when some platform can read it.
    pub fn supported(&self) -> bool {
        self.reader != Reader::None || self.windows.is_some()
    }

    /// Keel's own reader, for the docs table.
    pub fn reader_name(&self) -> &'static str {
        match self.reader {
            Reader::None => "",
            Reader::SignatureId | Reader::ContentType => "file signature",
            Reader::PerceivedType => "file signature, text check",
            Reader::ShortcutTarget => "symlink, `.lnk` parser",
            Reader::Url => "`.url` parser",
            Reader::Pe(_) => "PE version resource",
            Reader::Text(_) => "text stats",
            Reader::Hash(_) => "hash (streamed)",
            Reader::Audio(_) => "ID3v2/ID3v1, MPEG, FLAC, WAV",
        }
    }
}

use AudioField as A;
use Conv::{Kbps, PerThousand, Perceived, Same, Ticks};
use Group::{Date, Document, FileInfo, Image, Media};

const fn f(
    name: &'static str,
    aliases: &'static [&'static str],
    group: Group,
    reader: Reader,
    windows: Option<&'static str>,
    conv: Conv,
) -> Function {
    Function {
        name,
        aliases,
        group,
        reader,
        windows,
        conv,
    }
}

const fn win(name: &'static str, group: Group, canonical: &'static str) -> Function {
    f(name, &[], group, Reader::None, Some(canonical), Same)
}

const fn none(name: &'static str, group: Group) -> Function {
    f(name, &[], group, Reader::None, None, Same)
}

const fn audio(name: &'static str, field: AudioField, canonical: Option<&'static str>) -> Function {
    f(name, &[], Media, Reader::Audio(field), canonical, Same)
}

const fn pe(name: &'static str, field: PeField, canonical: &'static str) -> Function {
    f(
        name,
        &[],
        FileInfo,
        Reader::Pe(field),
        Some(canonical),
        Same,
    )
}

/// Every property function, in audit Part 1's order within each group.
#[rustfmt::skip]
pub static FUNCTIONS: &[Function] = &[
    // File info.
    f("perceived-type", &[], FileInfo, Reader::PerceivedType, Some("System.PerceivedType"), Perceived),
    f("content-type", &[], FileInfo, Reader::ContentType, Some("System.ContentType"), Same),
    none("document-content-type", FileInfo),
    f("file-signature", &[], FileInfo, Reader::SignatureId, None, Same),
    f("shortcut-target", &[], FileInfo, Reader::ShortcutTarget, Some("System.Link.TargetParsingPath"), Same),
    none("volume-label", FileInfo),
    none("location", FileInfo),
    win("computer", FileInfo, "System.ComputerName"),
    f("url", &[], FileInfo, Reader::Url, Some("System.Link.TargetUrl"), Same),
    win("owner", FileInfo, "System.FileOwner"),
    none("protected", FileInfo),
    win("offline-availability", FileInfo, "System.OfflineAvailability"),
    win("offline-status", FileInfo, "System.OfflineStatus"),
    none("run-count", FileInfo),
    none("is-running", FileInfo),
    win("shared-with", FileInfo, "System.SharedWith"),
    win("status", FileInfo, "System.Status"),
    win("content-status", FileInfo, "System.ContentStatus"),
    f("character-encoding", &[], FileInfo, Reader::Text(TextField::Encoding), None, Same),
    f("line-count", &[], FileInfo, Reader::Text(TextField::Lines), None, Same),
    f("word-count", &[], FileInfo, Reader::Text(TextField::Words), None, Same),
    win("page-count", FileInfo, "System.Document.PageCount"),
    win("slide-count", FileInfo, "System.Presentation.SlideCount"),
    f("total-editing-time", &[], FileInfo, Reader::None, Some("System.Document.TotalEditingTime"), Ticks),
    f("md5", &[], FileInfo, Reader::Hash(HashAlgo::Md5), None, Same),
    f("sha1", &[], FileInfo, Reader::Hash(HashAlgo::Sha1), None, Same),
    f("sha256", &[], FileInfo, Reader::Hash(HashAlgo::Sha256), None, Same),
    f("crc32", &[], FileInfo, Reader::Hash(HashAlgo::Crc32), None, Same),
    f("crc64", &["crc"], FileInfo, Reader::Hash(HashAlgo::Crc64), None, Same),
    none("digital-signature-name", FileInfo),
    none("digital-signature-timestamp", FileInfo),
    pe("version", PeField::FileVersion, "System.FileVersion"),
    pe("product-name", PeField::ProductName, "System.Software.ProductName"),
    pe("product-version", PeField::ProductVersion, "System.Software.ProductVersion"),
    pe("company", PeField::Company, "System.Company"),
    pe("copyright", PeField::Copyright, "System.Copyright"),
    pe("trademarks", PeField::Trademarks, "System.Trademarks"),
    pe("description", PeField::Description, "System.FileDescription"),
    none("details", FileInfo),
    // Dates held by properties (the file system dates come from the index).
    none("date-run", Date),
    win("date-deleted", Date, "System.Recycle.DateDeleted"),
    win("date-saved", Date, "System.Document.DateSaved"),
    win("date-printed", Date, "System.Document.DatePrinted"),
    win("date-received", Date, "System.Message.DateReceived"),
    win("date-sent", Date, "System.Message.DateSent"),
    win("date-released", Date, "System.Media.DateReleased"),
    win("date-acquired", Date, "System.DateAcquired"),
    none("date-changed", Date),
    win("date-content-created", Date, "System.Document.DateCreated"),
    win("date-taken", Date, "System.Photo.DateTaken"),
    f("date-encoded", &["date-media-created"], Date, Reader::None, Some("System.Media.DateEncoded"), Same),
    // Image.
    f("width", &["wide"], Image, Reader::None, Some("System.Image.HorizontalSize"), Same),
    f("height", &["high"], Image, Reader::None, Some("System.Image.VerticalSize"), Same),
    win("dimensions", Image, "System.Image.Dimensions"),
    win("orientation", Image, "System.Photo.Orientation"),
    f("bit-depth", &["bpp"], Image, Reader::None, Some("System.Image.BitDepth"), Same),
    win("horizontal-resolution", Image, "System.Image.HorizontalResolution"),
    win("vertical-resolution", Image, "System.Image.VerticalResolution"),
    none("aspect-ratio", Image),
    win("camera-maker", Image, "System.Photo.CameraManufacturer"),
    win("camera-model", Image, "System.Photo.CameraModel"),
    win("lens-model", Image, "System.Photo.LensModel"),
    win("aperture", Image, "System.Photo.Aperture"),
    win("f-stop", Image, "System.Photo.FNumber"),
    win("max-aperture", Image, "System.Photo.MaxAperture"),
    win("exposure-time", Image, "System.Photo.ExposureTime"),
    win("exposure-bias", Image, "System.Photo.ExposureBias"),
    win("exposure-program", Image, "System.Photo.ExposureProgram"),
    win("exif-version", Image, "System.Photo.EXIFVersion"),
    win("flash-mode", Image, "System.Photo.Flash"),
    win("focal-length", Image, "System.Photo.FocalLength"),
    win("35mm-focal-length", Image, "System.Photo.FocalLengthInFilm"),
    win("iso-speed", Image, "System.Photo.ISOSpeed"),
    win("light-source", Image, "System.Photo.LightSource"),
    win("metering-mode", Image, "System.Photo.MeteringMode"),
    win("brightness", Image, "System.Photo.Brightness"),
    win("contrast", Image, "System.Photo.Contrast"),
    win("saturation", Image, "System.Photo.Saturation"),
    win("white-balance", Image, "System.Photo.WhiteBalance"),
    win("subject-distance", Image, "System.Photo.SubjectDistance"),
    win("latitude", Image, "System.GPS.LatitudeDecimal"),
    win("longitude", Image, "System.GPS.LongitudeDecimal"),
    win("altitude", Image, "System.GPS.Altitude"),
    // Audio, video and music.
    audio("album", A::Album, Some("System.Music.AlbumTitle")),
    audio("album-artist", A::AlbumArtist, Some("System.Music.AlbumArtist")),
    audio("artist", A::Artist, Some("System.Music.Artist")),
    win("authors", Media, "System.Author"),
    audio("composer", A::Composer, Some("System.Music.Composer")),
    audio("conductor", A::Conductor, Some("System.Music.Conductor")),
    win("director", Media, "System.Video.Director"),
    win("producer", Media, "System.Media.Producer"),
    audio("writer", A::Writer, Some("System.Media.Writer")),
    audio("genre", A::Genre, Some("System.Music.Genre")),
    audio("mood", A::Mood, Some("System.Music.Mood")),
    audio("title", A::Title, Some("System.Title")),
    audio("subtitle", A::Subtitle, Some("System.Media.SubTitle")),
    audio("track", A::Track, Some("System.Music.TrackNumber")),
    audio("year", A::Year, Some("System.Media.Year")),
    f("beats-per-minute", &["bpm"], Media, Reader::Audio(A::Bpm), Some("System.Music.BeatsPerMinute"), Same),
    audio("initial-key", A::InitialKey, Some("System.Music.InitialKey")),
    f("length", &["duration"], Media, Reader::Audio(A::Length), Some("System.Media.Duration"), Ticks),
    f("audio-bitrate", &[], Media, Reader::Audio(A::Bitrate), Some("System.Audio.EncodingBitrate"), Kbps),
    audio("audio-bits-per-sample", A::BitsPerSample, Some("System.Audio.SampleSize")),
    audio("audio-channels", A::Channels, Some("System.Audio.ChannelCount")),
    audio("audio-format", A::Format, Some("System.Audio.Format")),
    audio("audio-sample-rate", A::SampleRate, Some("System.Audio.SampleRate")),
    none("audio-track-count", Media),
    f("video-bitrate", &[], Media, Reader::None, Some("System.Video.EncodingBitrate"), Kbps),
    win("video-format", Media, "System.Video.Compression"),
    none("video-track-count", Media),
    none("subtitle-track-count", Media),
    f("total-bitrate", &[], Media, Reader::None, Some("System.Video.TotalBitrate"), Kbps),
    none("frame-count", Media),
    f("frame-rate", &["fps"], Media, Reader::None, Some("System.Video.FrameRate"), PerThousand),
    audio("language", A::Language, Some("System.Language")),
    audio("part-of-a-compilation", A::Compilation, Some("System.Music.IsCompilation")),
    audio("part-of-set", A::PartOfSet, Some("System.Music.PartOfSet")),
    win("period", Media, "System.Music.Period"),
    audio("encoded-by", A::EncodedBy, Some("System.Media.EncodedBy")),
    audio("publisher", A::Publisher, Some("System.Media.Publisher")),
    win("parental-rating", Media, "System.ParentalRating"),
    win("parental-rating-reason", Media, "System.ParentalRatingReason"),
    // Document and other.
    win("subject", Document, "System.Subject"),
    f("comment", &[], Document, Reader::Audio(A::Comment), Some("System.Comment"), Same),
    win("category", Document, "System.Category"),
    f("tags", &["tag"], Document, Reader::None, Some("System.Keywords"), Same),
    win("rating", Document, "System.Rating"),
    win("people", Document, "System.Photo.PeopleNames"),
    win("last-author", Document, "System.Document.LastAuthor"),
    win("software", Document, "System.ApplicationName"),
    win("from", Document, "System.Message.FromAddress"),
    win("to", Document, "System.Message.ToAddress"),
];

/// The function `name` (or an alias) names, case-insensitive, with or without the colon.
pub fn lookup(name: &str) -> Option<&'static Function> {
    let n = name.trim().trim_end_matches(':');
    FUNCTIONS.iter().find(|f| {
        f.name.eq_ignore_ascii_case(n) || f.aliases.iter().any(|a| a.eq_ignore_ascii_case(n))
    })
}

/// True for a Windows Property System canonical name (`System.Music.Artist`), which
/// [`crate::get`] reads directly on Windows.
pub fn is_canonical_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let first = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    !first.is_empty()
        && first
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && !rest.is_empty()
        && rest.iter().all(|p| !p.is_empty())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

/// The docs table (`docs/properties.md`) for one group: function, aliases, own reader,
/// Windows property.
pub fn markdown_table(group: Group) -> String {
    let mut s = String::from("| Function | Keel's own reader (every platform) | Windows Property System |\n|---|---|---|\n");
    for f in FUNCTIONS.iter().filter(|f| f.group == group) {
        let mut name = format!("`{}:`", f.name);
        for a in f.aliases {
            name.push_str(&format!(" (`{a}:`)"));
        }
        let own = match f.reader_name() {
            "" => "no".to_owned(),
            r => r.to_owned(),
        };
        let win = f
            .windows
            .map_or_else(|| "no".to_owned(), |w| format!("`{w}`"));
        s.push_str(&format!("| {name} | {own} | {win} |\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_found() {
        let mut seen = std::collections::HashSet::new();
        for f in FUNCTIONS {
            for n in std::iter::once(&f.name).chain(f.aliases) {
                assert!(seen.insert(*n), "duplicate {n}");
                assert_eq!(lookup(n).unwrap().name, f.name);
                assert_eq!(
                    lookup(&format!("{}:", n.to_uppercase())).unwrap().name,
                    f.name
                );
            }
        }
        assert!(lookup("no-such-function").is_none());
        assert_eq!(
            lookup("crc:").unwrap().reader,
            Reader::Hash(HashAlgo::Crc64)
        );
        assert_eq!(lookup("Duration").unwrap().name, "length");
    }

    #[test]
    fn canonical_names() {
        assert!(is_canonical_name("System.Music.Artist"));
        assert!(is_canonical_name("System.Size"));
        assert!(!is_canonical_name("System."));
        assert!(!is_canonical_name("artist"));
        assert!(!is_canonical_name("a b.c"));
        assert!(!is_canonical_name(".x"));
    }

    /// The tables in docs/properties.md are generated from [`FUNCTIONS`].
    #[test]
    fn docs_table_is_current() {
        let docs =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/properties.md");
        let text = std::fs::read_to_string(&docs)
            .unwrap()
            .replace("\r\n", "\n");
        let mut missing = String::new();
        for g in [
            Group::FileInfo,
            Group::Date,
            Group::Image,
            Group::Media,
            Group::Document,
        ] {
            let table = markdown_table(g);
            if !text.contains(&table) {
                missing.push_str(&format!("### {}\n\n{table}\n", g.title()));
            }
        }
        assert!(
            missing.is_empty(),
            "docs/properties.md is out of date; expected:\n\n{missing}"
        );
    }
}
