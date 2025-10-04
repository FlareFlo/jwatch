use std::time::Duration;

#[derive(Debug)]
pub struct MediaInfo {
    pub duration: Duration,
    pub size: usize,
    pub bitrate: usize,
    pub height: usize,
    pub width: usize,
    pub codec: Codec,
}

#[derive(Debug)]
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