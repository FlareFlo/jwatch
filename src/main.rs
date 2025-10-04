use color_eyre::Report;
use color_eyre::eyre::ContextCompat;
use std::env;
use walkdir::WalkDir;

mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

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
            dbg!(&file.path());
            dbg!(mediainfo::get_mediainfo(file.path(),)?);
        }
    }
    Ok(())
}

