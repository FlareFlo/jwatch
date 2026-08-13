use std::fmt::{Display, Formatter};
use std::time::Duration;

use time::OffsetDateTime;

#[allow(unused)]
#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub duration: Duration,
    pub size: usize,
    pub bitrate: usize,
    pub height: usize,
    pub width: usize,
    pub codec: Codec,
    pub last_checked: OffsetDateTime,
    pub mtime: i64, // Last modification of file in seconds
    pub audio_language: Vec<LangTrack>,
    pub subtitle_languages: Vec<LangTrack>,
    pub whitelisted: bool,
}

#[derive(Debug, Clone)]
pub struct LangTrack {
    pub language: String,
    /// Stream size in bytes, 0 if the container carries no per-track statistics
    pub size: u64,
}

impl MediaInfo {
    pub fn megabitrate(&self) -> f64 {
        self.bitrate as f64 / 2.0_f64.powi(20)
    }
}

#[allow(unused)]
#[derive(Debug, Clone, PartialEq)]
pub enum Codec {
    H264,
    H265,
    AV1,
    Other(String),
}

impl Codec {
    /// Accepts three spellings per codec: mediainfo's `Format` field ("AVC"), the MP4
    /// fourcc ("avc1"), and our own `Display` output ("H264"). The last one matters
    /// because the cache stores `Display` and reads it back through here, matching only
    /// the fourccs made every named variant unreachable and the round-trip lossy.
    pub fn from_str(code: &str) -> Codec {
        const H264: &[&str] = &["AVC", "avc1", "H264"];
        const H265: &[&str] = &["HEVC", "hvc1", "H265"];
        const AV1: &[&str] = &["AV1", "av01"];

        let is = |alternatives: &[&str]| alternatives.iter().any(|a| a.eq_ignore_ascii_case(code));
        if is(H264) {
            Codec::H264
        } else if is(H265) {
            Codec::H265
        } else if is(AV1) {
            Codec::AV1
        } else {
            Codec::Other(code.to_owned())
        }
    }
}

impl Display for Codec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Codec::H264 => "H264",
            Codec::H265 => "H265",
            Codec::AV1 => "AV1",
            Codec::Other(other) => other.as_str(),
        }
        .fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mediainfo_format_names_map_to_named_variants() {
        // What the probe actually feeds us; verified against mediainfo output
        assert_eq!(Codec::from_str("AVC"), Codec::H264);
        assert_eq!(Codec::from_str("HEVC"), Codec::H265);
        assert_eq!(Codec::from_str("AV1"), Codec::AV1);
    }

    #[test]
    fn mp4_fourccs_still_map_to_named_variants() {
        assert_eq!(Codec::from_str("avc1"), Codec::H264);
        assert_eq!(Codec::from_str("hvc1"), Codec::H265);
        assert_eq!(Codec::from_str("av01"), Codec::AV1);
    }

    /// The cache stores `Display` output and reloads it through `from_str`, so every
    /// variant has to survive a round-trip
    #[test]
    fn display_round_trips_through_from_str() {
        for codec in [
            Codec::H264,
            Codec::H265,
            Codec::AV1,
            Codec::Other("VP9".to_owned()),
        ] {
            assert_eq!(Codec::from_str(&codec.to_string()), codec);
        }
    }

    #[test]
    fn unknown_codecs_are_preserved_verbatim() {
        assert_eq!(Codec::from_str("VP9"), Codec::Other("VP9".to_owned()));
        assert_eq!(Codec::from_str(""), Codec::Other(String::new()));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(Codec::from_str("avc"), Codec::H264);
        assert_eq!(Codec::from_str("hevc"), Codec::H265);
        assert_eq!(Codec::from_str("av1"), Codec::AV1);
    }
}
