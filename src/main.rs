use crate::mediainfo::get_mediainfo;
use color_eyre::Report;
use color_eyre::eyre::ContextCompat;
use std::sync::Arc;
use std::{env, thread};
use walkdir::WalkDir;

mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

pub const MBIT: usize = 2 ^ 20;

fn main() -> JwatchResult<()> {
    let path = env::args().nth(1).context("missing path to folder")?;
    let mut t = vec![];
    for file in WalkDir::new(&path).max_open(1) {
        let file = file?;
        if file.metadata()?.is_file() {
            // Skip common metadata and auxiliary media stored alongside the media were interested in
            if ["nfo", "srt", "jpg", "magnet"].contains(
                &file
                    .path()
                    .extension()
                    .context("missing file extension")?
                    .to_str()
                    .context("failed to convert ostr to str")?,
            ) {
                continue;
            }
            let res = thread::spawn(move || {
                dbg!(&file.path());
                let mediainfo = get_mediainfo(&file.path())?;

                if !(..20.0).contains(&mediainfo.megabitrate()) {
                    let filename = file.file_name().to_string_lossy();
                    eprintln!(
                        "{filename} bitrate is bad! {:.1} mbit/s",
                        mediainfo.megabitrate()
                    )
                }
                Ok::<(), Report>(())
            });
            t.push(res);
        }
    }
    for t in t {
        t.join().unwrap()?;
    }
    Ok(())
}
