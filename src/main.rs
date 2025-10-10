use crate::cachedb::init_cachedb;
use crate::mediainfo::get_mediainfo;
use color_eyre::{Report, Section};
use color_eyre::eyre::{Context, ContextCompat};
use indicatif::{ProgressBar, ProgressFinish, ProgressIterator, ProgressStyle};
use rusqlite::Connection;
use std::borrow::Cow;
use std::env;
use std::ffi::OsStr;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use walkdir::{DirEntry, WalkDir};

mod cachedb;
mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

pub const MBIT: usize = 2 ^ 20;

fn main() -> JwatchResult<()> {
    color_eyre::install()?;
    let path = env::args().nth(1).context("missing path to folder")?;
    let cachedb = Connection::open(path.clone() + "/jwatch.sqlite")?;
    init_cachedb(&cachedb)?;

    let start = Instant::now();
    let progress = ProgressBar::new_spinner()
        .with_message("Indexing media...")
        .with_elapsed(start.elapsed())
        .with_style(
            ProgressStyle::with_template("{spinner} T+{elapsed:<2} | {pos:<5} — {wide_msg}")?
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
        )
        .with_finish(ProgressFinish::WithMessage(Cow::Borrowed("indexed media")));

    let files: Vec<Result<PathBuf, _>> = WalkDir::new(&path)
        .into_iter()
        .map(|e| e.map(DirEntry::into_path))
        .filter(|path| {
            path.as_ref()
                .map(|p| {
                    ["mkv", "mp4", "avi", "mov", "flv", "wmv", "webm", "m4v"].contains(
                        &p.extension()
                            .unwrap_or_else(|| OsStr::new(""))
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .as_ref(),
                    )
                })
                .unwrap_or(false)
        })
        .progress_with(progress)
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
            progress.set_message(format!(
                "processing {}",
                path.file_name().context("missing file name")?.display()
            ));
            let mediainfo = get_mediainfo(&path, file.metadata()?, &cachedb).with_note(||format!("Occurred in: {}", path.display()))?;
            let filename = path
                .file_name()
                .context("missing file path")?
                .to_string_lossy()
                .to_string();

            if !(0.2..20.0).contains(&mediainfo.megabitrate()) {
                let reason =  format!(
                    "Undesired bitrate: {:<4.1} mbit/s with codec {:<4}",
                    mediainfo.megabitrate(),
                    mediainfo.codec,
                );
                reports.push((reason, filename.clone(), mediainfo.clone()));
            }
            let desired_langs = &["en", "de"];
            let undesired = mediainfo.languages.clone().into_iter().filter(|l|!desired_langs.contains(&l.as_str())).collect::<Vec<_>>();
            if undesired.len() > 0 {
                reports.push((format!("Undesired languages {}", undesired.join(" ")), filename.clone(), mediainfo.clone()));
            }
        }
    }
    for (reason, filename, _mediainfo) in reports {
       eprintln!("{} found in: {filename}", reason);
    }
    cachedb.close().map_err(|e|e.1).context("failed to close cachedb connection")?;
    Ok(())
}
