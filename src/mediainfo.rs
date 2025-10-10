use crate::JwatchResult;
use crate::cachedb::{get_from_cachedb, store_to_cachedb};
use crate::metastructs::{Codec, MediaInfo};
use color_eyre::eyre::{ContextCompat, bail, eyre};
use rusqlite::Connection;
use std::collections::HashMap;
use std::fs::Metadata;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};
use color_eyre::Help;
use time::OffsetDateTime;

pub fn get_mediainfo(
    p: impl AsRef<Path> + std::fmt::Debug,
    metadata: Metadata,
    cachedb: &Connection,
) -> JwatchResult<MediaInfo> {
    if let Some(info) = get_from_cachedb(&p, cachedb).note("Database needs migration?")?
        && info.mtime
            == metadata
                .modified()?
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs() as _ && false
    {
        return Ok(info);
    }

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
        .map_err(|e| eyre!("Invalid UTF-8 in mediainfo output: {}", e))?;
    let kv: Vec<(&str, HashMap<&str, &str>)> =
        Vec::from_iter(stdout.split("\n\n").map(|section| {
            let mut section = section.lines();
            let header = section.next().unwrap();
            let keys = HashMap::from_iter(section.map(|line| {
                let (key, value) = line.split_once(':').unwrap_or(("", ""));
                (key.trim(), value.trim())
            }));
            (header, keys)
        }));
    let getkey = |section, key| {
        kv.iter().find(|(k, _)|*k == section)
            .with_context(|| format!("missing section {section} in {p:?}"))?.1
            .get(key)
            .with_context(|| format!("missing key {key} in {p:?}"))
    };

    let info = MediaInfo {
        duration: Duration::from_secs_f64(getkey("General", "Duration")?.parse::<f64>()? / 1000.0),
        size: getkey("General", "FileSize")?.parse()?,
        bitrate: getkey("General", "OverallBitRate")?.parse()?,
        height: getkey("Video", "Height")?.parse()?,
        width: getkey("Video", "Width")?.parse()?,
        codec: Codec::from_str(getkey("Video", "Format")?),
        last_checked: OffsetDateTime::now_local()?,
        mtime: metadata
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs() as i64,
        languages: kv.iter().filter(|(k, _)|k.contains("Audio")).map(|(_, elem)|elem.get("Language").unwrap_or(&"").to_string()).collect::<Vec<_>>(),
        whitelisted: false,
    };
    store_to_cachedb(p, &info, cachedb)?;
    Ok(info)
}
