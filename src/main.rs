use crate::argparse::Args;
use crate::cachedb::CacheDB;
use crate::mediainfo::get_mediainfo;
use color_eyre::eyre::{bail, ContextCompat};
use color_eyre::Report;
use indicatif::{ProgressBar, ProgressFinish, ProgressIterator, ProgressStyle};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use walkdir::{DirEntry, WalkDir};

mod argparse;
mod cachedb;
mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "mov", "flv", "wmv", "webm", "m4v"];
const ACCEPTED_BITRATE_RANGE: std::ops::Range<f64> = 0.2..20.0;
/// Used for both Audio and Subtitle languages
const ACCEPTED_LANGS: &[&str] = &["en", "de"];

fn is_video_file(entry: &DirEntry) -> bool {
    entry
        .path()
        .extension()
        .map(OsStr::to_string_lossy)
        .map(|ext| {
            let ext = ext.to_ascii_lowercase();
            VIDEO_EXTENSIONS.contains(&ext.as_str())
        })
        .unwrap_or(false)
}

fn main() -> JwatchResult<()> {
    color_eyre::install()?;
    let args: Args = argh::from_env();
    let path = args.path;
    let cachedb = CacheDB::init_cachedb(args.db_path.as_deref().unwrap_or(&path))?; // TODO: DEDUP

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
        .filter(|e| e.as_ref().map(is_video_file).unwrap_or(false))
        .map(|e| e.map(DirEntry::into_path))
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
    let mut errors = 0u32;
    for path in files.into_iter().progress_with(progress.clone()) {
        let path = path?;
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                progress.println(format!("stat: {}: {}", e, path.display()));
                errors += 1;
                continue;
            }
        };
        if metadata.is_file() {
            progress.set_message(format!(
                "processing {}",
                path.file_name().context("missing file name")?.display()
            ));
            let mediainfo = match get_mediainfo(&path, metadata, &cachedb) {
                Ok(m) => m,
                Err(e) => {
                    progress.println(format!("mediainfo: {:?}: {}", e, path.display()));
                    errors += 1;
                    continue;
                }
            };
            let filename = path
                .file_name()
                .context("missing file path")?
                .to_string_lossy()
                .to_string();

            if !ACCEPTED_BITRATE_RANGE.contains(&mediainfo.megabitrate()) {
                let reason = format!(
                    "Undesired bitrate: {:<4.1} mbit/s with codec {:<4}",
                    mediainfo.megabitrate(),
                    mediainfo.codec,
                );
                reports.push((reason, filename.clone(), mediainfo.clone()));
            }

            let desired_langs = ACCEPTED_LANGS;
            let undesired = mediainfo
                .languages
                .clone()
                .into_iter()
                .filter(|l| !desired_langs.contains(&l.as_str()))
                .collect::<Vec<_>>();
            if !undesired.is_empty() {
                reports.push((
                    format!("Undesired languages {}", undesired.join(" ")),
                    filename.clone(),
                    mediainfo.clone(),
                ));
            }

            let undesired_subs = mediainfo
                .subtitle_languages
                .clone()
                .into_iter()
                .filter(|l| !desired_langs.contains(&l.as_str()))
                .collect::<Vec<_>>();
            if !undesired_subs.is_empty() {
                reports.push((
                    format!("Undesired subtitle languages {}", undesired_subs.join(" ")),
                    filename.clone(),
                    mediainfo.clone(),
                ));
            }
        }
    }

    for (reason, filename, _mediainfo) in reports {
        println!("{} found in: {filename}", reason);
    }

    cachedb.cleanup()?;

    if errors > 0 {
        bail!("{errors} file(s) failed to process");
    }

    Ok(())
}
