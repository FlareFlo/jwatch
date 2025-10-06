use crate::JwatchResult;
use crate::metastructs::Codec;
use crate::metastructs::MediaInfo;
use color_eyre::eyre::{ContextCompat};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::Duration;
use time::OffsetDateTime;

pub fn init_cachedb(cachedb: &Connection) -> JwatchResult<()> {
    cachedb.execute(
        "\
	CREATE TABLE IF NOT EXISTS media (
	path TEXT PRIMARY KEY,
	duration INTEGER NOT NULL,
	size INTEGER NOT NULL,
	bitrate INTEGER NOT NULL,
	height INTEGER NOT NULL,
	width INTEGER NOT NULL,
	codec TEXT NOT NULL,
    last_checked INTEGER NOT NULL
	)",
        (),
    )?;
    Ok(())
}

pub fn get_from_cachedb(
    p: impl AsRef<Path>,
    cachedb: &Connection,
) -> JwatchResult<Option<MediaInfo>> {
    let res = cachedb
        .query_one(
            "
		SELECT path, duration, size, bitrate, height, width, codec, last_checked
		FROM media
		WHERE path = ?1
	",
            params![p.as_ref().file_name().context("missing filename")?.to_string_lossy()],
            |row| {
                Ok(MediaInfo {
                    duration: Duration::from_millis(row.get(1)?),
                    size: row.get(2)?,
                    bitrate: row.get(3)?,
                    height: row.get(4)?,
                    width: row.get(5)?,
                    codec: Codec::from_str(row.get_ref(6)?.as_str()?),
                    last_checked: OffsetDateTime::from_unix_timestamp(row.get(7)?).unwrap(),
                })
            },
        )
        .optional()?;
    Ok(res)
}

pub fn store_to_cachedb(
    p: impl AsRef<Path>,
    media_info: &MediaInfo,
    cachedb: &Connection,
) -> JwatchResult<()> {
    cachedb.execute(
        "\
	INSERT OR REPLACE INTO media
	(path, duration, size, bitrate, height, width, codec, last_checked)
	VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
	",
        (
            p.as_ref().file_name().context("missing filename")?.to_string_lossy(),
            media_info.duration.as_millis() as i64,
            media_info.size,
            media_info.bitrate,
            media_info.height,
            media_info.width,
            media_info.codec.to_string(),
            media_info.last_checked.unix_timestamp(),
        ),
    )?;
    Ok(())
}
