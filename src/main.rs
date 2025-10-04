use color_eyre::Report;
use color_eyre::eyre::ContextCompat;
use std::env;
use walkdir::WalkDir;
use crate::mediainfo::get_mediainfo;

mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

pub const MBIT: usize = 2^20;

fn main() -> JwatchResult<()> {
    let path = env::args().nth(1).context("missing path to folder")?;
    for file in WalkDir::new(&path) {
        let file = file?;
        if file.metadata()?.is_file() {
            if ["nfo", "srt"].contains(
                &file
                    .path()
                    .extension()
                    .context("missing file extension")?
                    .to_str()
                    .context("failed to convert ostr to str")?,
            ) {
                continue;
            }
            let mediainfo = get_mediainfo(&file.path())?;

            if !(..20.0).contains(&mediainfo.megabitrate()) {
                let filename = file.file_name().to_string_lossy();
                eprintln!("{filename} bitrate is bad! {:.1} mbit/s", mediainfo.megabitrate())
            }

        }
    }
    Ok(())
}

