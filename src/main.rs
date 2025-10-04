use color_eyre::Report;
use color_eyre::eyre::{ContextCompat, bail, eyre};
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use walkdir::WalkDir;

pub type JwatchResult<T> = Result<T, Report>;

fn main() -> JwatchResult<()> {
    let path = env::args().nth(1).context("missing path to folder")?;
    for file in WalkDir::new(&path) {
        let file = file?;
        if file.metadata()?.is_file() {
            if file.path().extension() == Some(OsStr::new("nfo")) {
                continue;
            }
            dbg!(&file.path());
            dbg!(get_mediainfo(file.path(),)?);
        }
    }
    Ok(())
}

fn get_mediainfo(p: impl AsRef<Path>) -> JwatchResult<MediaInfo> {
    let cmd = Command::new("mediainfo")
        .arg("--Language=raw")
        .arg("--Full")
        .arg(p.as_ref())
        .output()?;

    if !cmd.status.success() {
        bail!(
            "mediainfo failed with status {:?}, stderr: {}",
            cmd.status.code(),
            String::from_utf8_lossy(&cmd.stdout)
        );
    }

    let stdout = String::from_utf8(cmd.stdout)
        .map_err(|e| (eyre!("Invalid UTF-8 in mediainfo output: {}", e)))?;
    let kv: HashMap<&str, &str> = HashMap::from_iter(stdout.lines().map(|line| {
        let (key, value) = line.split_once(':').unwrap_or(("", ""));
        (key.trim(), value.trim())
    }));
    Ok(MediaInfo {
        duration: Duration::from_secs_f64(kv.get("Duration").unwrap().parse::<f64>()? / 1000.0),
        size: kv.get("FileSize").unwrap().parse()?,
        bitrate: kv.get("OverallBitRate").unwrap().parse()?,
        height: kv.get("Height").unwrap().parse()?,
        width: kv.get("Width").unwrap().parse()?,
        codec: Codec::from_str(kv["CodecID"]),
    })
}

#[derive(Debug)]
pub struct MediaInfo {
    duration: Duration,
    size: usize,
    bitrate: usize,
    height: usize,
    width: usize,
    codec: Codec,
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
