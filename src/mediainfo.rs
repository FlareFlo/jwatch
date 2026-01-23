use crate::JwatchResult;
use crate::cachedb::{get_from_cachedb, store_to_cachedb};
use crate::metastructs::{Codec, MediaInfo};
use color_eyre::eyre::{ContextCompat, bail, eyre};
use rusqlite::Connection;
use serde::Deserialize;
use std::fs::Metadata;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};
use color_eyre::Help;
use time::OffsetDateTime;

#[derive(Deserialize)]
struct JsonMediaInfo {
    media: JsonMedia,
}

#[derive(Deserialize)]
struct JsonMedia {
    #[serde(rename = "track")]
    tracks: Vec<Track>,
}


#[derive(Deserialize)]
struct Track {
    #[serde(rename = "@type")]
    type_: String,
    /// Duration in seconds.milliseconds (9010.001000000 for 2h30min10s and 1ms)
    #[serde(rename = "Duration")]
    duration: Option<String>,
    #[serde(rename = "FileSize")]
    file_size: Option<String>,
    #[serde(rename = "OverallBitRate")]
    overall_bit_rate: Option<String>,
    #[serde(rename = "Width")]
    width: Option<String>,
    #[serde(rename = "Height")]
    height: Option<String>,
    #[serde(rename = "Format")]
    format: Option<String>,
    #[serde(rename = "Language")]
    language: Option<String>,
}

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
                .as_secs() as i64
    {
        return Ok(info);
    }


    let cmd = Command::new("mediainfo")
        .arg("--Language=raw")
        .arg("--Full")
        .arg("--Output=JSON")
        .arg(p.as_ref())
        .output()?;

    if !cmd.status.success() {
        bail!(
            "mediainfo failed with status {:?}, stderr: {}",
            cmd.status.code(),
            String::from_utf8_lossy(&cmd.stdout)
        );
    }

    let output = String::from_utf8(cmd.stdout)
        .map_err(|e| eyre!("Invalid UTF-8 in mediainfo output: {}", e))?;

    let json: JsonMediaInfo = serde_json::from_str(&output)
        .map_err(|e| eyre!("Failed to parse mediainfo JSON output: {}", e))?;
    let tracks = json.media.tracks;

    let general_track = tracks.iter().find(|t| t.type_ == "General")
        .with_context(|| format!("missing General track in mediainfo output for {p:?}"))?;

    let video_track = tracks.iter().find(|t| t.type_ == "Video")
        .with_context(|| format!("missing Video track in mediainfo output for {p:?}"))?;

    
    let info = MediaInfo {
        duration: Duration::from_secs_f64(
            general_track.duration
                .as_ref()
                .with_context(|| format!("missing Duration in General track for {p:?}"))?
                .parse::<f64>()?,
        ),
        size: general_track.file_size
            .as_ref()
            .with_context(|| format!("missing FileSize in General track for {p:?}"))?
            .parse()?,
        bitrate: general_track.overall_bit_rate
            .as_ref()
            .with_context(|| format!("missing OverallBitRate in General track for {p:?}"))?
            .parse()?,
        height: video_track.height
            .as_ref()
            .with_context(|| format!("missing Height in Video track for {p:?}"))?
            .parse()?,
        width: video_track.width
            .as_ref()
            .with_context(|| format!("missing Width in Video track for {p:?}"))?
            .parse()?,
        codec: Codec::from_str(
            video_track.format
                .as_ref()
                .with_context(|| format!("missing Format in Video track for {p:?}"))?,
        ),
        last_checked: OffsetDateTime::now_local()?,
        mtime: metadata
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs() as i64,
        languages: tracks.iter().filter(|t| t.type_ == "Audio").filter_map(|t| t.language.clone()).collect::<Vec<_>>(),
        whitelisted: false,
    };

    store_to_cachedb(p, &info, cachedb)?;

    Ok(info)
}
