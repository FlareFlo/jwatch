use crate::JwatchResult;
use crate::metastructs::Codec;
use crate::metastructs::MediaInfo;
use color_eyre::eyre::{Context, ContextCompat};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::Path;
use std::time::Duration;
use time::OffsetDateTime;

const DB_APP_ID: i32 = i32::from_le_bytes([b'j', b'w', b'a', b't']);

pub fn init_cachedb(mut cachedb: &mut Connection, path: String) -> JwatchResult<()> {
    let dbschema = //language=sqlite
        "\
	CREATE TABLE IF NOT EXISTS media (
	path TEXT PRIMARY KEY,
	duration INTEGER NOT NULL,
	size INTEGER NOT NULL,
	bitrate INTEGER NOT NULL,
	height INTEGER NOT NULL,
	width INTEGER NOT NULL,
	codec TEXT NOT NULL,
    last_checked INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    languages TEXT NOT NULL,
    whitelisted BOOLEAN NOT NULL
	)";
    let mut h = DefaultHasher::new();
    h.write(dbschema.as_bytes());
    let hash = h.finish() as i32; // Yes this truncates a bit, doesnt matter though.
    let dbhash = cachedb.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if hash != dbhash {
        eprintln!("DB schema out of date, migrating...");
        fs::remove_file(&path)?;
        *cachedb = Connection::open(&path)?;
        cachedb.pragma_update(None, "application_id", &hash)?;
    }
    cachedb.pragma_update(None, "user_version", &hash)?;

    cachedb.execute(dbschema, ())?;
    Ok(())
}

pub fn get_from_cachedb(
    p: impl AsRef<Path>,
    cachedb: &Connection,
) -> JwatchResult<Option<MediaInfo>> {
    let res = cachedb
        .query_one(
            //language=sqlite
            "
		SELECT path, duration, size, bitrate, height, width, codec, last_checked, mtime, languages, whitelisted
		FROM media
		WHERE path = ?1
	",
            params![
                p.as_ref()
                    .file_name()
                    .context("missing filename")?
                    .to_string_lossy()
            ],
            |row| {
                Ok(MediaInfo {
                    duration: Duration::from_millis(row.get(1)?),
                    size: row.get(2)?,
                    bitrate: row.get(3)?,
                    height: row.get(4)?,
                    width: row.get(5)?,
                    codec: Codec::from_str(row.get_ref(6)?.as_str()?),
                    last_checked: OffsetDateTime::from_unix_timestamp(row.get(7)?).unwrap(),
                    mtime: row.get(8)?,
                    languages: row.get::<_, String>(9)?.split(' ').map(str::to_owned).collect(),
                    whitelisted: row.get(10)?,
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
        //language=sqlite
        "\
	INSERT OR REPLACE INTO media
	(path, duration, size, bitrate, height, width, codec, last_checked, mtime, languages, whitelisted)
	VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
	",
        (
            p.as_ref()
                .file_name()
                .context("missing filename")?
                .to_string_lossy(),
            media_info.duration.as_millis() as i64,
            media_info.size,
            media_info.bitrate,
            media_info.height,
            media_info.width,
            media_info.codec.to_string(),
            media_info.last_checked.unix_timestamp(),
            media_info.mtime,
            media_info.languages.join(" "),
            media_info.whitelisted,
        ),
    )?;
    Ok(())
}
