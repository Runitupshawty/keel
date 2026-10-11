//! Audio files: tags, duration and format from `ffprobe` (`keel_core::ffprobe_json`), the
//! embedded cover art and a waveform through `ffmpeg`. Playback is the app's (the video
//! player's sound path, without a picture).

use crate::{Preview, Request, Rgba};
use std::path::Path;
use std::process::Command;

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "opus", "m4a", "aac"];
pub(crate) const NEEDS_FFMPEG: &str = "Install ffmpeg to see audio details and play audio";
/// Drawing the waveform decodes the whole file.
// ponytail: files past 20 minutes get no waveform, so the preview stays inside its
// timeout; draw one from a sample of the file if long recordings want it.
const WAVE_MAX_SECONDS: f64 = 20.0 * 60.0;
/// The waveform's colour: readable on the dark and the light theme.
const WAVE_COLOUR: &str = "0x4f8fdf";

pub(crate) fn accepts(ext: &str) -> bool {
    AUDIO_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request) -> Preview {
    let Some(ffmpeg) = keel_core::find_tool("ffmpeg") else {
        return Preview::Missing(NEEDS_FFMPEG);
    };
    let probe = match keel_core::ffprobe_json(&req.bytes_path) {
        Ok(probe) => probe,
        Err(e) => return Preview::Error(format!("{e:#}")),
    };
    let Some(facts) = facts(&probe) else {
        return Preview::Error("no audio stream".into());
    };
    let px = req.max_px.clamp(64, 2048);
    let cover = facts.cover.and_then(|stream| {
        let scale =
            format!("scale=w='min(iw,{px})':h='min(ih,{px})':force_original_aspect_ratio=decrease");
        let map = format!("0:{stream}");
        picture(&ffmpeg, &req.bytes_path, &["-map", &map, "-vf", &scale])
    });
    let wave = (facts.duration_s <= WAVE_MAX_SECONDS)
        .then(|| {
            let (w, h) = (px, (px / 5).clamp(48, 160));
            let graph = format!(
                "[0:a:0]aformat=channel_layouts=mono,showwavespic=s={w}x{h}:colors={WAVE_COLOUR}[w]"
            );
            picture(
                &ffmpeg,
                &req.bytes_path,
                &["-filter_complex", &graph, "-map", "[w]"],
            )
        })
        .flatten();
    Preview::Audio {
        cover,
        wave,
        duration_s: facts.duration_s,
        tags: facts.tags,
        meta: facts.meta,
    }
}

/// What the panel shows of an audio file, from ffprobe's JSON.
#[derive(Debug, PartialEq)]
pub(crate) struct Facts {
    pub duration_s: f64,
    /// (label, value) for title, artist and album, those present.
    pub tags: Vec<(String, String)>,
    /// "flac · 44.1 kHz · stereo · 912 kb/s".
    pub meta: String,
    /// The stream index of the cover art (an attached picture).
    pub cover: Option<u64>,
}

/// None when the file has no audio stream.
pub(crate) fn facts(v: &serde_json::Value) -> Option<Facts> {
    let streams = v["streams"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let audio = streams.iter().find(|s| s["codec_type"] == "audio")?;
    let format = &v["format"];
    // Tag keys differ in case (Vorbis comments are upper case); Ogg and Opus keep them on
    // the stream, the other containers on the format.
    let tag = |key: &str| {
        [&format["tags"], &audio["tags"]]
            .into_iter()
            .find_map(|tags| {
                tags.as_object()?
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .and_then(|(_, v)| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            })
    };
    let tags = [("Title", "title"), ("Artist", "artist"), ("Album", "album")]
        .into_iter()
        .filter_map(|(label, key)| Some((label.to_owned(), tag(key)?)))
        .collect();
    let number = |v: &serde_json::Value| {
        v.as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| v.as_f64())
            .filter(|n| n.is_finite() && *n > 0.0)
    };
    let duration_s = number(&format["duration"])
        .or_else(|| number(&audio["duration"]))
        .unwrap_or(0.0);
    let mut meta: Vec<String> = audio["codec_name"]
        .as_str()
        .map(str::to_owned)
        .into_iter()
        .collect();
    if let Some(rate) = number(&audio["sample_rate"]) {
        meta.push(format!("{} kHz", (rate / 100.0).round() / 10.0));
    }
    match audio["channels"].as_u64() {
        Some(1) => meta.push("mono".into()),
        Some(2) => meta.push("stereo".into()),
        Some(n) if n > 2 => meta.push(format!("{n} channels")),
        _ => {}
    }
    if let Some(bits) = number(&format["bit_rate"]).or_else(|| number(&audio["bit_rate"])) {
        meta.push(format!("{} kb/s", (bits / 1000.0).round()));
    }
    let cover = streams
        .iter()
        .find(|s| s["disposition"]["attached_pic"].as_i64() == Some(1))
        .and_then(|s| s["index"].as_u64());
    Some(Facts {
        duration_s,
        tags,
        meta: meta.join(" · "),
        cover,
    })
}

/// One picture ffmpeg makes from `path` with `args` (PNG on a pipe), or None.
fn picture(ffmpeg: &Path, path: &Path, args: &[&str]) -> Option<Rgba> {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(args)
        .args(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"]);
    let (status, png, _) = crate::video::run(command).ok()?;
    if !status.success() || png.is_empty() {
        return None;
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    // At most 2048 x 2048 was asked for.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 << 20);
    reader.limits(limits);
    let rgba = reader.decode().ok()?.to_rgba8();
    Some(Rgba {
        w: rgba.width(),
        h: rgba.height(),
        data: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tags_format_and_cover_from_ffprobe() {
        let flac = json!({
            "streams": [
                {"index": 0, "codec_type": "audio", "codec_name": "flac", "sample_rate": "44100",
                 "channels": 2},
                {"index": 1, "codec_type": "video", "codec_name": "png",
                 "disposition": {"attached_pic": 1}}
            ],
            "format": {"duration": "61.500000", "bit_rate": "912345",
                       "tags": {"TITLE": "Test Tone", "ARTIST": "Keel", "ALBUM": " "}}
        });
        assert_eq!(
            facts(&flac),
            Some(Facts {
                duration_s: 61.5,
                tags: vec![
                    ("Title".into(), "Test Tone".into()),
                    ("Artist".into(), "Keel".into())
                ],
                meta: "flac · 44.1 kHz · stereo · 912 kb/s".into(),
                cover: Some(1),
            })
        );
        // Opus keeps its tags on the stream; no format bitrate, a mono stream.
        let opus = json!({
            "streams": [{"index": 0, "codec_type": "audio", "codec_name": "opus",
                         "sample_rate": "48000", "channels": 1, "bit_rate": "64000",
                         "tags": {"title": "Note"}}],
            "format": {"duration": "N/A"}
        });
        let f = facts(&opus).unwrap();
        assert_eq!(f.tags, vec![("Title".into(), "Note".into())]);
        assert_eq!(f.meta, "opus · 48 kHz · mono · 64 kb/s");
        assert_eq!((f.duration_s, f.cover), (0.0, None));
        assert_eq!(facts(&json!({"streams": [{"codec_type": "video"}]})), None);
        assert_eq!(facts(&json!({})), None);
    }
}
