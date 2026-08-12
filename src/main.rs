use crate::argparse::Args;
use crate::cachedb::CacheDB;
use crate::mediainfo::probe_mediainfo;
use crate::metastructs::MediaInfo;
use color_eyre::Report;
use color_eyre::eyre::{ContextCompat, bail, eyre};
use indicatif::{HumanBytes, ProgressBar, ProgressFinish, ProgressIterator, ProgressStyle};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};
use walkdir::{DirEntry, WalkDir};

mod argparse;
mod cachedb;
mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "mov", "flv", "wmv", "webm", "m4v"];
const ACCEPTED_BITRATE_RANGE: std::ops::Range<f64> = 0.2..20.0;
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
    let jobs = args.jobs.max(1);
    // --db-path names the exact db file; by default it lives inside the scanned folder
    let db_file = args
        .db_path
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&path).join("jwatch.sqlite"));
    let cachedb = CacheDB::init_cachedb(&db_file)?;

    // The handler runs on its own thread and cannot touch the (!Sync) db connection,
    // so it only raises a flag; the loops below stop on it, and the normal
    // report/summary/cleanup path persists what we have.
    let interrupted = Arc::new(AtomicBool::new(false));
    ctrlc::set_handler({
        let interrupted = interrupted.clone();
        let mut first_interrupt: Option<Instant> = None;
        move || {
            match first_interrupt {
                // Deliberate second CTRL+C: give up on graceful shutdown. The debounce
                // matters: one keypress can deliver SIGINT twice in quick succession
                // (e.g. to both the process and its group), which must not force-quit.
                Some(t) if t.elapsed() > Duration::from_millis(300) => std::process::exit(130),
                Some(_) => {}
                None => {
                    first_interrupt = Some(Instant::now());
                    interrupted.store(true, Ordering::Relaxed);
                    eprintln!("Interrupted, stopping scan (CTRL+C again to force quit)...");
                    eprintln!("Saving current process to database...");
                }
            }
        }
    })?;

    let start = Instant::now();
    let progress = ProgressBar::new_spinner()
        .with_message("Indexing media...")
        .with_elapsed(start.elapsed())
        .with_style(
            ProgressStyle::with_template("{spinner} T+{elapsed:<2} | {pos:<5} — {wide_msg}")?
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
        )
        .with_finish(ProgressFinish::WithMessage(Cow::Borrowed("indexed media")));

    let files: Vec<PathBuf> = WalkDir::new(&path)
        .into_iter()
        .take_while(|_| !interrupted.load(Ordering::Relaxed))
        .filter(|e| e.as_ref().map(is_video_file).unwrap_or(false))
        .map(|e| e.map(DirEntry::into_path))
        .progress_with(progress)
        .collect::<Result<_, _>>()?;

    let start = Instant::now();
    let progress = ProgressBar::new(files.len() as u64)
        .with_elapsed(start.elapsed())
        .with_style(ProgressStyle::with_template(
            "{spinner} T+{elapsed:<2} T-{eta:<2} {bar:60.cyan/red} {pos:>5}/{len:<5} {wide_msg}"
        )?.tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"))
        .with_finish(ProgressFinish::WithMessage(Cow::Borrowed("processed all media")));
    progress.enable_steady_tick(Duration::from_millis(50));

    let cache = cachedb.load_all()?;

    let mut results: Vec<Option<MediaInfo>> = Vec::new();
    results.resize_with(files.len(), || None);
    let mut errors = 0u32;
    let mut files_total = 0u64;

    let next_file = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let tx = tx.clone();
            let progress = progress.clone();
            let interrupted = interrupted.clone();
            let (files, cache, next_file) = (&files, &cache, &next_file);
            scope.spawn(move || {
                loop {
                    if interrupted.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next_file.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    if tx.send((i, probe_one(path, cache, &progress))).is_err() {
                        break;
                    }
                }
            });
        }
        // The workers hold the remaining senders; the loop below ends when they finish
        drop(tx);

        // All DB access stays on this thread — the connection is !Sync
        for (i, outcome) in rx {
            progress.inc(1);
            match outcome {
                ProbeOutcome::Skipped => {}
                ProbeOutcome::Cached(info) => {
                    files_total += 1;
                    results[i] = Some(info);
                }
                ProbeOutcome::Fresh(info) => {
                    files_total += 1;
                    if let Err(e) = cachedb.store_to_cachedb(&files[i], &info) {
                        progress.println(format!("cachedb: {:?}: {}", e, files[i].display()));
                        errors += 1;
                    }
                    results[i] = Some(info);
                }
                ProbeOutcome::Failed(e) => {
                    if interrupted.load(Ordering::Relaxed) {
                        // The terminal delivers SIGINT to the mediainfo children too,
                        // so failures after the interrupt are our own doing, not bad files
                        continue;
                    }
                    files_total += 1;
                    progress.println(format!("{:?}: {}", e, files[i].display()));
                    errors += 1;
                }
            }
        }
    });
    progress.finish_using_style();

    let mut reports = vec![];
    let mut files_non_ideal = 0u64;
    let mut saved_video = 0u64;
    let mut saved_audio = 0u64;
    let mut saved_subs = 0u64;
    for (path, mediainfo) in files.iter().zip(&results) {
        let Some(mediainfo) = mediainfo else {
            continue;
        };
        {
            let reports_before = reports.len();
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
                if mediainfo.megabitrate() >= ACCEPTED_BITRATE_RANGE.end {
                    let max_bytes_per_sec = ACCEPTED_BITRATE_RANGE.end * 2.0_f64.powi(20) / 8.0;
                    let bytes_per_sec = mediainfo.bitrate as f64 / 8.0;
                    saved_video += ((bytes_per_sec - max_bytes_per_sec)
                        * mediainfo.duration.as_secs_f64())
                        as u64;
                }
                reports.push((reason, filename.clone(), mediainfo.clone()));
            }

            let desired_langs = ACCEPTED_LANGS;
            let undesired = mediainfo
                .audio_language
                .iter()
                .filter(|t| !desired_langs.contains(&t.language.as_str()))
                .collect::<Vec<_>>();
            if !undesired.is_empty() {
                saved_audio += undesired.iter().map(|t| t.size).sum::<u64>();
                let langs = undesired
                    .iter()
                    .map(|t| t.language.as_str())
                    .collect::<Vec<_>>();
                reports.push((
                    format!("Undesired languages {}", langs.join(" ")),
                    filename.clone(),
                    mediainfo.clone(),
                ));
            }

            let undesired_subs = mediainfo
                .subtitle_languages
                .iter()
                .filter(|t| !desired_langs.contains(&t.language.as_str()))
                .collect::<Vec<_>>();
            if !undesired_subs.is_empty() {
                saved_subs += undesired_subs.iter().map(|t| t.size).sum::<u64>();
                let langs = undesired_subs
                    .iter()
                    .map(|t| t.language.as_str())
                    .collect::<Vec<_>>();
                reports.push((
                    format!("Undesired subtitle languages {}", langs.join(" ")),
                    filename.clone(),
                    mediainfo.clone(),
                ));
            }

            if reports.len() > reports_before {
                files_non_ideal += 1;
            }
        }
    }

    for (reason, filename, _mediainfo) in reports {
        println!("{} found in: {filename}", reason);
    }

    if interrupted.load(Ordering::Relaxed) {
        println!("Scan interrupted, results are partial");
    }
    println!("Summary:");
    println!("\tNon-ideal files: {files_non_ideal}/{files_total}");
    println!("\tMinimum savings:");
    println!("\t\tVideo:     {}", HumanBytes(saved_video));
    println!("\t\tAudio:     {}", HumanBytes(saved_audio));
    println!("\t\tSubtitles: {}", HumanBytes(saved_subs));
    println!(
        "\t\tTotal:     {}",
        HumanBytes(saved_video + saved_audio + saved_subs)
    );

    cachedb.cleanup()?;

    if errors > 0 {
        bail!("{errors} file(s) failed to process");
    }
    if interrupted.load(Ordering::Relaxed) {
        // Conventional exit code for SIGINT
        std::process::exit(130);
    }

    Ok(())
}

enum ProbeOutcome {
    /// Not a regular file
    Skipped,
    /// Served from the preloaded cache
    Cached(MediaInfo),
    /// Probed with mediainfo, still needs storing
    Fresh(MediaInfo),
    Failed(Report),
}

/// Runs on worker threads: stat, cache lookup, mediainfo probe. No DB access.
fn probe_one(
    path: &Path,
    cache: &HashMap<String, MediaInfo>,
    progress: &ProgressBar,
) -> ProbeOutcome {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return ProbeOutcome::Failed(eyre!("stat: {e}")),
    };
    if !metadata.is_file() {
        return ProbeOutcome::Skipped;
    }
    if let Some(name) = path.file_name() {
        progress.set_message(format!("processing {}", name.display()));
    }

    let mtime = match metadata
        .modified()
        .map_err(Report::new)
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).map_err(Report::new))
    {
        Ok(d) => d.as_secs() as i64,
        Err(e) => return ProbeOutcome::Failed(e),
    };
    if let Some(info) = path
        .file_name()
        .and_then(|n| cache.get(&*n.to_string_lossy()))
        && info.mtime == mtime
    {
        return ProbeOutcome::Cached(info.clone());
    }

    match probe_mediainfo(path, &metadata) {
        Ok(info) => ProbeOutcome::Fresh(info),
        Err(e) => ProbeOutcome::Failed(e),
    }
}
