use crate::argparse::Args;
use crate::cachedb::CacheDB;
use crate::lang::{is_undefined_lang, is_undesired_lang};
use crate::mediainfo::probe_mediainfo;
use crate::metastructs::MediaInfo;
use color_eyre::Report;
use color_eyre::eyre::{ContextCompat, bail, eyre};
use console::style;
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
mod fix;
mod lang;
mod mediainfo;
mod metastructs;

pub type JwatchResult<T> = Result<T, Report>;

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "mov", "flv", "wmv", "webm", "m4v"];
const ACCEPTED_BITRATE_RANGE: std::ops::Range<f64> = 0.2..20.0;

enum Level {
    /// Something we would act on, counted as a non-ideal file
    Undesired,
    /// Informational only: nothing here can be fixed automatically
    Warning,
}

struct Finding {
    level: Level,
    reason: String,
    filename: String,
}

impl Finding {
    fn undesired(reason: String, filename: String) -> Self {
        Self {
            level: Level::Undesired,
            reason,
            filename,
        }
    }

    fn warning(reason: String, filename: String) -> Self {
        Self {
            level: Level::Warning,
            reason,
            filename,
        }
    }
}

/// Our own scratch files; the name ends in .mkv, so extension matching alone claims them
fn is_stale_tmp(entry: &DirEntry) -> bool {
    !entry.file_type().is_dir() && entry.file_name().to_string_lossy().contains(".jwatch-tmp.")
}

fn is_video_file(entry: &DirEntry) -> bool {
    !entry.file_type().is_dir()
        && entry
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
    let run_fix = args.fix || args.dry_run;
    if args.apply.is_some() && !run_fix {
        bail!("--apply requires --fix (or --dry-run to preview)");
    }
    let path = &args.path;
    let jobs = args.jobs.max(1);
    // --db-path names the exact db file; by default it lives inside the scanned folder
    let db_file = args
        .db_path
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(path).join("jwatch.sqlite"));
    let cachedb = CacheDB::init_cachedb(&db_file)?;

    let fix_backend = if run_fix {
        Some(fix::detect_backend()?)
    } else {
        None
    };

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

    let mut stale_tmp: Vec<PathBuf> = vec![];
    let files: Vec<PathBuf> = WalkDir::new(path)
        .into_iter()
        .take_while(|_| !interrupted.load(Ordering::Relaxed))
        // Stale first; a temp file also matches is_video_file
        .filter_map(|e| match e {
            Err(e) => Some(Err(e)),
            Ok(entry) if is_stale_tmp(&entry) => {
                stale_tmp.push(entry.into_path());
                None
            }
            Ok(entry) if is_video_file(&entry) => Some(Ok(entry.into_path())),
            Ok(_) => None,
        })
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

        // All DB access stays on this thread; the connection is !Sync
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

    let mut findings: Vec<Finding> = vec![];
    let mut fix_candidates: Vec<(&PathBuf, &MediaInfo)> = vec![];
    let mut non_mkv_fixables = 0u32;
    let mut files_non_ideal = 0u64;
    let mut saved_video = 0u64;
    let mut saved_audio = 0u64;
    let mut saved_subs = 0u64;
    for (path, mediainfo) in files.iter().zip(&results) {
        let Some(mediainfo) = mediainfo else {
            continue;
        };
        let filename = path
            .file_name()
            .context("missing file path")?
            .to_string_lossy()
            .to_string();
        // Warnings are informational, so they must not make a file count as non-ideal
        let mut has_defect = false;

        if !ACCEPTED_BITRATE_RANGE.contains(&mediainfo.megabitrate()) {
            let reason = format!(
                "Undesired bitrate: {:<4.1} mbit/s with codec {:<4}",
                mediainfo.megabitrate(),
                mediainfo.codec,
            );
            if mediainfo.megabitrate() >= ACCEPTED_BITRATE_RANGE.end {
                let max_bytes_per_sec = ACCEPTED_BITRATE_RANGE.end * 2.0_f64.powi(20) / 8.0;
                let bytes_per_sec = mediainfo.bitrate as f64 / 8.0;
                saved_video +=
                    ((bytes_per_sec - max_bytes_per_sec) * mediainfo.duration.as_secs_f64()) as u64;
            }
            findings.push(Finding::undesired(reason, filename.clone()));
            has_defect = true;
        }

        let undesired = mediainfo
            .audio_language
            .iter()
            .filter(|t| is_undesired_lang(Some(&t.language)))
            .collect::<Vec<_>>();
        if !undesired.is_empty() {
            saved_audio += undesired.iter().map(|t| t.size).sum::<u64>();
            let langs = undesired
                .iter()
                .map(|t| t.language.as_str())
                .collect::<Vec<_>>();
            findings.push(Finding::undesired(
                format!("Undesired languages {}", langs.join(" ")),
                filename.clone(),
            ));
            has_defect = true;
        }

        let undesired_subs = mediainfo
            .subtitle_languages
            .iter()
            .filter(|t| is_undesired_lang(Some(&t.language)))
            .collect::<Vec<_>>();
        if !undesired_subs.is_empty() {
            saved_subs += undesired_subs.iter().map(|t| t.size).sum::<u64>();
            let langs = undesired_subs
                .iter()
                .map(|t| t.language.as_str())
                .collect::<Vec<_>>();
            findings.push(Finding::undesired(
                format!("Undesired subtitle languages {}", langs.join(" ")),
                filename.clone(),
            ));
            has_defect = true;
        }

        // Untagged tracks are never stripped, so these are for the user to retag by hand
        let count_undefined = |tracks: &[metastructs::LangTrack]| {
            tracks
                .iter()
                .filter(|t| is_undefined_lang(Some(&t.language)))
                .count()
        };
        let undefined_audio = count_undefined(&mediainfo.audio_language);
        if undefined_audio > 0 {
            findings.push(Finding::warning(
                format!("undefined audio language on {undefined_audio} track(s)"),
                filename.clone(),
            ));
        }
        let undefined_subs = count_undefined(&mediainfo.subtitle_languages);
        if undefined_subs > 0 {
            findings.push(Finding::warning(
                format!("undefined subtitle language on {undefined_subs} track(s)"),
                filename.clone(),
            ));
        }

        if !undesired.is_empty() || !undesired_subs.is_empty() {
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
            {
                fix_candidates.push((path, mediainfo));
            } else {
                non_mkv_fixables += 1;
            }
        }

        if has_defect {
            files_non_ideal += 1;
        }
    }

    for p in &stale_tmp {
        findings.push(Finding::warning(
            "leftover temp file from an interrupted fix run".to_owned(),
            p.display().to_string(),
        ));
    }

    for finding in &findings {
        match finding.level {
            Level::Undesired => println!("{} found in: {}", finding.reason, finding.filename),
            // style() strips the escapes when stdout is not a terminal
            Level::Warning => println!(
                "{}",
                style(format!("Warning: {}: {}", finding.reason, finding.filename)).yellow()
            ),
        }
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

    // Commit cache before the fix pass, which can fail for unrelated reasons
    cachedb.cleanup()?;

    let mut fix_errors = 0u32;
    let mut skipped = 0u32;
    if let Some(backend) = fix_backend {
        // Deliberately sequential and off the worker pool: each remux is a full
        // read+write of the file, so more than one at a time just thrashes the disk
        let execute = args.apply.filter(|_| !args.dry_run);

        if fix_candidates.is_empty() {
            println!("Fix: nothing to do");
        } else {
            println!(
                "Fix pass ({} file(s), via {}):",
                fix_candidates.len(),
                backend.name()
            );
            let mut fixed = 0u32;
            let mut fix_delta = 0i64;
            for (path, mediainfo) in fix_candidates {
                if interrupted.load(Ordering::Relaxed) {
                    break;
                }
                let plan = match fix::plan_fix(path, mediainfo, backend) {
                    Ok(Some(plan)) => plan,
                    Ok(None) => {
                        println!(
                            "\tnothing to strip, guards kept every track: {}",
                            path.display()
                        );
                        continue;
                    }
                    Err(e) => {
                        if interrupted.load(Ordering::Relaxed) {
                            // mkvmerge -J and mediainfo take the same SIGINT we did
                            println!("\tinterrupted while planning");
                        } else {
                            println!("\tplanning failed: {:?}: {}", e, path.display());
                            fix_errors += 1;
                        }
                        continue;
                    }
                };
                println!("\t{}", plan.command_line());
                for note in &plan.notes {
                    println!("\t\tnote: {note}");
                }
                let links = fix::hardlinks(path);
                if links > 1 {
                    println!(
                        "{}",
                        style(format!(
                            "\t\twarning: {links} hardlinks, remuxing unshares this copy so the original stays on disk"
                        ))
                        .yellow()
                    );
                }
                if let Some(mode) = execute {
                    match fix::apply_fix(&plan, mode) {
                        Ok(fix::FixOutcome::Fixed(delta)) => {
                            fixed += 1;
                            fix_delta += delta;
                            if delta >= 0 {
                                println!("\t\tfixed, saved {}", HumanBytes(delta as u64));
                            } else {
                                println!("\t\tfixed, grew by {}", HumanBytes(delta.unsigned_abs()));
                            }
                        }
                        Ok(fix::FixOutcome::SkippedTempExists(tmp)) => {
                            skipped += 1;
                            println!(
                                "{}",
                                style(format!(
                                    "\t\tskipped, leftover temp file in the way: {}",
                                    tmp.display()
                                ))
                                .yellow()
                            );
                        }
                        Ok(fix::FixOutcome::SkippedBackupExists(bak)) => {
                            skipped += 1;
                            println!(
                                "{}",
                                style(format!(
                                    "\t\tskipped, backup already exists: {}",
                                    bak.display()
                                ))
                                .yellow()
                            );
                        }
                        Err(e) => {
                            if interrupted.load(Ordering::Relaxed) {
                                // The terminal delivers SIGINT to the remux child too, so
                                // this failure is our own doing, not a bad file
                                println!("\t\tinterrupted, original left untouched");
                            } else {
                                println!("\t\tfailed: {e:?}");
                                fix_errors += 1;
                            }
                        }
                    }
                }
            }
            if execute.is_some() {
                if fix_delta >= 0 {
                    println!(
                        "Fixed {fixed} file(s), net saved {}",
                        HumanBytes(fix_delta as u64)
                    );
                } else {
                    println!(
                        "Fixed {fixed} file(s), net grew by {}",
                        HumanBytes(fix_delta.unsigned_abs())
                    );
                }
                if fix_errors > 0 {
                    println!("{fix_errors} file(s) could not be fixed");
                }
            } else {
                println!(
                    "Dry run: no files were modified. To apply, add --apply backup (keep originals) or --apply replace (overwrite)."
                );
            }
        }
        if non_mkv_fixables > 0 {
            println!(
                "{non_mkv_fixables} file(s) with undesired tracks skipped: --fix only supports mkv"
            );
        }
    }

    let mut problems = vec![];
    if errors > 0 {
        problems.push(format!("{errors} file(s) failed to process"));
    }
    if fix_errors > 0 {
        problems.push(format!("{fix_errors} file(s) failed to fix"));
    }
    if skipped > 0 {
        problems.push(format!("{skipped} file(s) skipped"));
    }
    if !problems.is_empty() {
        bail!("{}", problems.join(", "));
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

    let mtime = match metadata.modified().map_err(Report::new).and_then(|m| {
        m.duration_since(SystemTime::UNIX_EPOCH)
            .map_err(Report::new)
    }) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder can carry a name matching our scratch pattern; only real files are
    /// leftovers, and the folder must still be walked normally.
    #[test]
    fn only_files_count_as_stale_scratch_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("season.jwatch-tmp.mkv")).unwrap();
        std::fs::write(dir.path().join("season.jwatch-tmp.mkv/ep1.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("a.mkv.jwatch-tmp.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"x").unwrap();

        let mut stale = vec![];
        let mut videos = vec![];
        for entry in WalkDir::new(dir.path()).into_iter().map(Result::unwrap) {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_stale_tmp(&entry) {
                stale.push(name);
            } else if is_video_file(&entry) {
                videos.push(name);
            }
        }
        stale.sort();
        videos.sort();

        assert_eq!(stale, ["a.mkv.jwatch-tmp.mkv"]);
        assert_eq!(videos, ["ep1.mkv", "movie.mkv"]);
    }
}
