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
    pub languages: Vec<String>,
    pub whitelisted: bool,
}

impl MediaInfo {
    pub fn megabitrate(&self) -> f64 {
        self.bitrate as f64 / 2.0_f64.powi(20)
    }
}

#[allow(unused)]
#[derive(Debug, Clone)]
pub enum Codec {
    H264,
    H265,
    AV1,
    Other(String),
}

impl Codec {
    pub fn from_str(code: &str) -> Codec {
        match code {
            "avc1" => Codec::H264,
            "hvc1" => Codec::H265,
            "av01" => Codec::AV1,
            _ => Codec::Other(code.to_owned()),
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
