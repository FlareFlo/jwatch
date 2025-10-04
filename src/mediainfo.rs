use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use color_eyre::eyre::{bail, eyre};
use crate::JwatchResult;
use crate::metastructs::{Codec, MediaInfo};

pub fn get_mediainfo(p: impl AsRef<Path>) -> JwatchResult<MediaInfo> {
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
    let kv: HashMap<&str, HashMap<&str, &str>> =
        HashMap::from_iter(stdout.split("\n\n").map(|section| {
            let mut section = section.lines();
            let header = section.next().unwrap();
            let keys = HashMap::from_iter(section.map(|line| {
                let (key, value) = line.split_once(':').unwrap_or(("", ""));
                (key.trim(), value.trim())
            }));
            (header, keys)
        }));
    Ok(MediaInfo {
        duration: Duration::from_secs_f64(kv["General"]["Duration"].parse::<f64>()? / 1000.0),
        size: kv["General"]["FileSize"].parse()?,
        bitrate: kv["General"]["OverallBitRate"].parse()?,
        height: kv["Video"]["Height"].parse()?,
        width: kv["Video"]["Width"].parse()?,
        codec: Codec::from_str(kv["Video"]["Format"]),
    })
}