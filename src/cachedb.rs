use crate::JwatchResult;
use crate::metastructs::Codec;
use crate::metastructs::MediaInfo;
use color_eyre::eyre::{Context, ContextCompat, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use time::OffsetDateTime;

const DB_APP_ID: i32 = i32::from_le_bytes([b'j', b'w', b'a', b't']);

#[derive(Clone)]
pub struct CacheDB {
    // We derive Debug here, so all new fields must
    connection: Arc<Connection>,
}

impl CacheDB {
    pub fn init_cachedb(path: &str) -> JwatchResult<Self> {
        let mut selbst = Self {
            connection: Arc::new(Connection::open(path.to_owned() + "jwatch.sqlite")?),
        };
        let db_app_id: i32 /* Type inference somehow thinks this should be !*/ = selbst.connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let schema_version: i32 =
            selbst
                .connection
                .pragma_query_value(None, "schema_version", |row| row.get(0))?;
        if db_app_id != DB_APP_ID && schema_version != 0 {
            // Schema 0 means the DB is uninitialized
            panic!(
                "Database app ID missmatch, refusing to touch it\nIf youre confident it is the correct one, you can manually delete it at {path}"
            );
        }

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
        let dbhash: i32 = selbst
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;

        if hash != dbhash {
            eprintln!("DB schema out of date, migrating...");
            fs::remove_file(&path)?;
            selbst.connection = Arc::new(Connection::open(&path)?);
            selbst
                .connection
                .pragma_update(None, "application_id", &DB_APP_ID)?;
        }
        selbst
            .connection
            .pragma_update(None, "user_version", &hash)?;

        selbst.connection.execute(dbschema, ())?;
        Ok(selbst)
    }

    pub fn get_from_cachedb(&self, p: impl AsRef<Path>) -> JwatchResult<Option<MediaInfo>> {
        let res = self.connection
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
        &self,
        p: impl AsRef<Path>,
        media_info: &MediaInfo,
    ) -> JwatchResult<()> {
        self.connection.execute(
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

    /// Not just drop due to error handling
    pub fn cleanup(mut self) -> JwatchResult<()> {
        let mut attempt = 0;
        // Manual loop because for cannot be broken out of
        // Attempt to drop DB for 10 seconds once every second
        let conn = loop {
            if attempt >= 10 {
                bail!("Failed to drop DB. Is another thread holding on to it?")
            }

            match Arc::try_unwrap(self.connection) {
                Ok(connection) => {
                    break connection;
                }
                Err(conn) => {
                    eprintln!("Failed to close DB. Attempt {attempt} out of 10");
                    self.connection = conn;
                }
            }
            sleep(Duration::from_millis(1000));
            attempt += 1;
        };

        conn.close()
            .map_err(|e| e.1)
            .context("failed to close cachedb connection")?;

        Ok(())
    }
}
