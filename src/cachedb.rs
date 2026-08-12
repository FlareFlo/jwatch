use crate::JwatchResult;
use crate::metastructs::Codec;
use crate::metastructs::{LangTrack, MediaInfo};
use color_eyre::eyre::{Context, ContextCompat, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::cell::Cell;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use time::OffsetDateTime;

const DB_APP_ID: i32 = i32::from_le_bytes([b'j', b'w', b'a', b't']);
/// Stores are grouped into transactions of this many INSERTs to avoid a commit+fsync per file
const STORE_BATCH_SIZE: u32 = 64;

/// Space-separated `lang:size` pairs, e.g. "en:123456 fr:0"
fn serialize_lang_tracks(tracks: &[LangTrack]) -> String {
    tracks
        .iter()
        .map(|t| format!("{}:{}", t.language, t.size))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_lang_tracks(s: &str) -> Vec<LangTrack> {
    s.split(' ')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (language, size) = p.rsplit_once(':').unwrap_or((p, "0"));
            LangTrack {
                language: language.to_owned(),
                size: size.parse().unwrap_or(0),
            }
        })
        .collect()
}

#[derive(Clone)]
pub struct CacheDB {
    // We derive Debug here, so all new fields must
    connection: Arc<Connection>,
    // Beware: a clone() gets its own counter while sharing the connection's transaction state
    pending_stores: Cell<u32>,
}

impl CacheDB {
    /// `db_file` is the exact database file to open/create
    pub fn init_cachedb(db_file: &Path) -> JwatchResult<Self> {
        let mut connection = Connection::open(db_file)?;
        let db_app_id: i32 /* Type inference somehow thinks this should be !*/ = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let schema_version: i32 =
            connection.pragma_query_value(None, "schema_version", |row| row.get(0))?;
        if db_app_id != DB_APP_ID && schema_version != 0 {
            // Schema 0 means the DB is uninitialized
            panic!(
                "Database app ID mismatch, refusing to touch it\nIf you're confident it is the correct one, you can manually delete it at {}",
                db_file.display()
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
    audio_tracks TEXT NOT NULL,
    subtitle_tracks TEXT NOT NULL,
    whitelisted BOOLEAN NOT NULL
	)";
        let mut hasher = DefaultHasher::new();
        hasher.write(dbschema.as_bytes());
        let hash = hasher.finish() as i32; // Yes this truncates a bit, doesn't matter though.
        let dbhash: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if hash != dbhash {
            if dbhash != 0 {
                // user_version 0 means the DB was just created, nothing to migrate
                eprintln!("DB schema out of date, migrating...");
            } else {
                eprintln!("Fresh db created, migrating...");
            }
            connection
                .close()
                .map_err(|e| e.1)
                .context("failed to close cachedb while migrating")?;
            fs::remove_file(db_file)?;
            connection = Connection::open(db_file)?;
            connection.pragma_update(None, "application_id", &DB_APP_ID)?;
        }
        connection.pragma_update(None, "user_version", &hash)?;

        connection.execute(dbschema, ())?;

        // journal_mode returns a result row, so plain pragma_update would fail
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;

        Ok(Self {
            connection: Arc::new(connection),
            pending_stores: Cell::new(0),
        })
    }

    pub fn get_from_cachedb(&self, p: impl AsRef<Path>) -> JwatchResult<Option<MediaInfo>> {
        let res = self.connection
            .query_one(
                //language=sqlite
                "
		SELECT path, duration, size, bitrate, height, width, codec, last_checked, mtime, audio_tracks, subtitle_tracks, whitelisted
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
                        audio_language: parse_lang_tracks(&row.get::<_, String>(9)?),
                        subtitle_languages: parse_lang_tracks(&row.get::<_, String>(10)?),
                        whitelisted: row.get(11)?,
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
        if self.connection.is_autocommit() {
            // Running BEGIN switches out of autocommit mode and starts the batch
            self.connection.execute_batch("BEGIN")?;
        }
        self.connection.execute(
            //language=sqlite
            "\
	INSERT OR REPLACE INTO media
	(path, duration, size, bitrate, height, width, codec, last_checked, mtime, audio_tracks, subtitle_tracks, whitelisted)
	VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
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
                serialize_lang_tracks(&media_info.audio_language),
                serialize_lang_tracks(&media_info.subtitle_languages),
                media_info.whitelisted,
            ),
        )?;

        let pending = self.pending_stores.get() + 1;
        if pending >= STORE_BATCH_SIZE {
            self.connection.execute_batch("COMMIT")?;
            self.pending_stores.set(0);
        } else {
            self.pending_stores.set(pending);
        }
        Ok(())
    }

    /// Not just drop due to error handling
    pub fn cleanup(mut self) -> JwatchResult<()> {
        if !self.connection.is_autocommit() {
            // Persist the partial store batch
            self.connection.execute_batch("COMMIT")?;
        }

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
