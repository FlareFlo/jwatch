use crate::mediainfo::get_mediainfo;
use color_eyre::Report;
use color_eyre::eyre::ContextCompat;
use indicatif::{ProgressBar, ProgressFinish, ProgressIterator, ProgressStyle};
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{env};
use std::borrow::Cow;
use walkdir::{DirEntry, WalkDir};

mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

pub const MBIT: usize = 2 ^ 20;

fn main() -> JwatchResult<()> {
    let path = env::args().nth(1).context("missing path to folder")?;

    let start = Instant::now();
    let progress = ProgressBar::new_spinner()
        .with_message("Indexing media...")
        .with_elapsed(start.elapsed())
        .with_style(ProgressStyle::with_template(
            "{spinner} T+{elapsed:<2} | {pos:<5} — {wide_msg}",
        )?.tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"))
        .with_finish(ProgressFinish::WithMessage(Cow::Borrowed("indexed media")));

    let files: Vec<Result<PathBuf, _>> = WalkDir::new(&path)
        .into_iter()
        .progress_with(progress)
        .map(|e| e.map(DirEntry::into_path))
        .collect();

    let start = Instant::now();
    let progress = ProgressBar::new(files.len() as u64)
        .with_elapsed(start.elapsed())
        .with_style(ProgressStyle::with_template(
            "{spinner} T+{elapsed:<2} T-{eta:<2} {bar:60.cyan/red} {pos:>5}/{len:<5} {wide_msg}"
        )?.tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"))
        .with_finish(ProgressFinish::WithMessage(Cow::Borrowed("processed all media")));
    progress.enable_steady_tick(Duration::from_millis(50));

    let mut reports = vec![];
    for path in files.into_iter().progress_with(progress.clone()) {
        let path = path?;
        let file = File::open(&path)?;
        if file.metadata()?.is_file() {
            // Skip common metadata and auxiliary media stored alongside the media were interested in
            if ["nfo", "srt", "jpg", "magnet"].contains(
                &path
                    .extension()
                    .context("missing file extension")?
                    .to_str()
                    .context("failed to convert ostr to str")?,
            ) {
                continue;
            }
            progress.set_message(format!("processing {}", path.file_name().context("missing file name")?.display()));
            let mediainfo = get_mediainfo(&path)?;

            if !(..20.0).contains(&mediainfo.megabitrate()) {
                let filename = path
                    .file_name()
                    .context("missing file path")?
                    .to_string_lossy().to_string();
                reports.push((filename, mediainfo));
            }
        }
    }
    for (filename, mediainfo) in reports {
        eprintln!(
            "Too high bitrate: {:<4.1} mbit/s in {:<4} Path: {filename}",
            mediainfo.megabitrate(),
            mediainfo.codec.to_string(), // Due to formatting
        )
    }
    Ok(())
}
