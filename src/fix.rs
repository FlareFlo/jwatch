use crate::JwatchResult;
use crate::lang::{is_undefined_lang, is_undesired_lang};
use crate::mediainfo::{TrackKind, probe_mediainfo, probe_track_layout};
use crate::metastructs::MediaInfo;
use color_eyre::eyre::{Context, bail, eyre};
use serde::Deserialize;
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

#[derive(Clone, Copy, PartialEq)]
pub enum FixBackend {
    Mkvmerge,
    Ffmpeg,
}

impl FixBackend {
    pub fn name(self) -> &'static str {
        match self {
            FixBackend::Mkvmerge => "mkvmerge",
            FixBackend::Ffmpeg => "ffmpeg",
        }
    }

    fn version_arg(self) -> &'static str {
        match self {
            FixBackend::Mkvmerge => "--version",
            FixBackend::Ffmpeg => "-version",
        }
    }

    fn is_available(self) -> bool {
        Command::new(self.name())
            .arg(self.version_arg())
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ApplyMode {
    /// Keep the original as <name>.jwatch-bak
    Backup,
    /// Overwrite the original
    Replace,
}

impl argh::FromArgValue for ApplyMode {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "backup" => Ok(ApplyMode::Backup),
            "replace" => Ok(ApplyMode::Replace),
            other => Err(format!(r#"expected "backup" or "replace", got "{other}""#)),
        }
    }
}

/// (selector, language), where the selector is whatever the backend uses to address the
/// track: an mkvmerge track id, or an ffmpeg per-type stream index
type TrackRef = (u64, Option<String>);

pub struct FixPlan {
    path: PathBuf,
    tmp: PathBuf,
    backend: FixBackend,
    args: Vec<String>,
    /// Expected duration of the remuxed file, for verification
    duration: std::time::Duration,
    pub notes: Vec<String>,
}

/// Quotes what a POSIX shell would mangle. The printed command is documentation; we never
/// run it through a shell.
fn shell_quote(arg: &str) -> Cow<'_, str> {
    let safe = |c: char| c.is_ascii_alphanumeric() || "._-/=:,+@".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        Cow::Borrowed(arg)
    } else {
        // Close the quote, emit an escaped one, reopen
        Cow::Owned(format!("'{}'", arg.replace('\'', r"'\''")))
    }
}

/// What became of one file. A skip is not an error; the run continues.
#[derive(Debug, PartialEq)]
pub enum FixOutcome {
    /// Bytes saved, negative if the remux grew the file
    Fixed(i64),
    SkippedTempExists(PathBuf),
    SkippedBackupExists(PathBuf),
}

impl FixPlan {
    pub fn command_line(&self) -> String {
        let mut parts = vec![self.backend.name().to_owned()];
        parts.extend(self.args.iter().map(|a| shell_quote(a).into_owned()));
        parts.join(" ")
    }
}

/// mkvmerge signals warnings with exit 1 and still produces valid output; 2 is a real error
fn mkvmerge_ok(status: &ExitStatus) -> bool {
    status.success() || status.code() == Some(1)
}

/// mkvmerge is preferred: it is mkv-native and reports languages already normalized
pub fn detect_backend() -> JwatchResult<FixBackend> {
    [FixBackend::Mkvmerge, FixBackend::Ffmpeg]
        .into_iter()
        .find(|b| b.is_available())
        .ok_or_else(|| eyre!("--fix requires mkvmerge or ffmpeg, neither was found in PATH"))
}

fn tmp_path(path: &Path) -> JwatchResult<PathBuf> {
    let filename = path
        .file_name()
        .ok_or_else(|| eyre!("missing filename"))?
        .to_string_lossy();
    Ok(path.with_file_name(format!("{filename}.jwatch-tmp.mkv")))
}

/// Computes which tracks to keep, or None if the guards leave nothing to strip.
fn keep_lists(
    audio: &[TrackRef],
    subs: &[TrackRef],
    notes: &mut Vec<String>,
) -> Option<(Vec<u64>, Vec<u64>)> {
    let mut keep_audio: Vec<u64> = audio
        .iter()
        .filter(|(_, lang)| !is_undesired_lang(lang.as_deref()))
        .map(|(id, _)| *id)
        .collect();
    if keep_audio.is_empty() && !audio.is_empty() {
        // Never produce a silent file
        notes.push("all audio tracks have undesired languages, keeping them all".to_owned());
        keep_audio = audio.iter().map(|(id, _)| *id).collect();
    }

    let keep_subs: Vec<u64> = subs
        .iter()
        .filter(|(_, lang)| !is_undesired_lang(lang.as_deref()))
        .map(|(id, _)| *id)
        .collect();

    if keep_audio.len() == audio.len() && keep_subs.len() == subs.len() {
        return None;
    }
    Some((keep_audio, keep_subs))
}

#[derive(Deserialize)]
struct MkvIdentify {
    tracks: Vec<MkvTrack>,
}

#[derive(Deserialize)]
struct MkvTrack {
    id: u64,
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    properties: MkvTrackProps,
}

#[derive(Deserialize, Default)]
struct MkvTrackProps {
    language: Option<String>,
    language_ietf: Option<String>,
}

impl MkvTrackProps {
    /// Prefer the BCP-47 tag, it is the more precise of the two.
    fn lang(&self) -> Option<String> {
        match self.language_ietf.as_deref() {
            Some(ietf) if !is_undefined_lang(Some(ietf)) => Some(ietf.to_owned()),
            _ => self.language.clone(),
        }
    }
}

fn mkvmerge_args(
    tmp: &Path,
    src: &Path,
    audio: &[TrackRef],
    subs: &[TrackRef],
    notes: &mut Vec<String>,
) -> Option<Vec<String>> {
    let (keep_audio, keep_subs) = keep_lists(audio, subs, notes)?;

    let join = |ids: &[u64]| ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let mut args = vec!["-o".to_owned(), tmp.to_string_lossy().into_owned()];
    if keep_audio.len() < audio.len() {
        args.extend(["--audio-tracks".to_owned(), join(&keep_audio)]);
    }
    if keep_subs.is_empty() && !subs.is_empty() {
        args.push("--no-subtitles".to_owned());
    } else if keep_subs.len() < subs.len() {
        args.extend(["--subtitle-tracks".to_owned(), join(&keep_subs)]);
    }
    args.push(src.to_string_lossy().into_owned());
    Some(args)
}

fn plan_fix_mkvmerge(path: &Path, info: &MediaInfo) -> JwatchResult<Option<FixPlan>> {
    let out = Command::new("mkvmerge").arg("-J").arg(path).output()?;
    if !mkvmerge_ok(&out.status) {
        bail!(
            "mkvmerge -J failed with status {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout)
        );
    }
    let identify: MkvIdentify =
        serde_json::from_slice(&out.stdout).context("failed to parse mkvmerge -J output")?;

    let tracks_of = |kind: &str| -> Vec<TrackRef> {
        identify
            .tracks
            .iter()
            .filter(|t| t.type_ == kind)
            .map(|t| (t.id, t.properties.lang()))
            .collect()
    };
    let audio = tracks_of("audio");
    let subs = tracks_of("subtitles");

    let mut notes = vec![];
    let tmp = tmp_path(path)?;
    let Some(args) = mkvmerge_args(&tmp, path, &audio, &subs, &mut notes) else {
        return Ok(None);
    };

    Ok(Some(FixPlan {
        path: path.to_owned(),
        tmp,
        backend: FixBackend::Mkvmerge,
        args,
        duration: info.duration,
        notes,
    }))
}

fn ffmpeg_args(
    tmp: &Path,
    src: &Path,
    audio: &[TrackRef],
    subs: &[TrackRef],
    notes: &mut Vec<String>,
) -> Option<Vec<String>> {
    let (keep_audio, keep_subs) = keep_lists(audio, subs, notes)?;

    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-i"]
        .map(str::to_owned)
        .into();
    args.push(src.to_string_lossy().into_owned());
    args.extend(["-map".to_owned(), "0:v".to_owned()]);
    if keep_audio.len() == audio.len() {
        args.extend(["-map".to_owned(), "0:a?".to_owned()]);
    } else {
        for n in &keep_audio {
            args.extend(["-map".to_owned(), format!("0:a:{n}")]);
        }
    }
    if keep_subs.len() == subs.len() && !subs.is_empty() {
        args.extend(["-map".to_owned(), "0:s?".to_owned()]);
    } else {
        for n in &keep_subs {
            args.extend(["-map".to_owned(), format!("0:s:{n}")]);
        }
    }
    // Attachments (fonts) and data streams must survive the remux
    args.extend(["-map".to_owned(), "0:t?".to_owned()]);
    args.extend(["-map".to_owned(), "0:d?".to_owned()]);
    args.extend(["-c".to_owned(), "copy".to_owned()]);
    args.push(tmp.to_string_lossy().into_owned());
    Some(args)
}

fn plan_fix_ffmpeg(path: &Path, info: &MediaInfo) -> JwatchResult<Option<FixPlan>> {
    // ffmpeg per-type stream specifiers (0:a:n) follow file order, matching mediainfo's
    let layout = probe_track_layout(path)?;
    let tracks_of = |kind: TrackKind| -> Vec<TrackRef> {
        layout
            .iter()
            .filter(|t| t.kind == kind)
            .enumerate()
            .map(|(i, t)| (i as u64, t.language.clone()))
            .collect()
    };
    let audio = tracks_of(TrackKind::Audio);
    let subs = tracks_of(TrackKind::Text);

    let mut notes = vec![];
    let tmp = tmp_path(path)?;
    let Some(args) = ffmpeg_args(&tmp, path, &audio, &subs, &mut notes) else {
        return Ok(None);
    };

    Ok(Some(FixPlan {
        path: path.to_owned(),
        tmp,
        backend: FixBackend::Ffmpeg,
        args,
        duration: info.duration,
        notes,
    }))
}

pub fn plan_fix(
    path: &Path,
    info: &MediaInfo,
    backend: FixBackend,
) -> JwatchResult<Option<FixPlan>> {
    match backend {
        FixBackend::Mkvmerge => plan_fix_mkvmerge(path, info),
        FixBackend::Ffmpeg => plan_fix_ffmpeg(path, info),
    }
}

/// Remuxing writes a new inode, so a file sharing links with e.g. a torrent copy stops being
/// shared and the library grows by a full copy. Stat failures report 1 to stay quiet; a file
/// we cannot read fails loudly enough during the remux.
pub fn hardlinks(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).map(|m| m.nlink()).unwrap_or(1)
}

fn backup_path(path: &Path) -> PathBuf {
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{filename}.jwatch-bak"))
}

/// Runs the remux, verifies the result, and swaps it in.
pub fn apply_fix(plan: &FixPlan, mode: ApplyMode) -> JwatchResult<FixOutcome> {
    // Checked before the remux so a conflict costs no work. Both renames below overwrite
    // silently, so anything already sitting on these names would be destroyed.
    if plan.tmp.exists() {
        return Ok(FixOutcome::SkippedTempExists(plan.tmp.clone()));
    }
    let bak = backup_path(&plan.path);
    if mode == ApplyMode::Backup && bak.exists() {
        return Ok(FixOutcome::SkippedBackupExists(bak));
    }

    let cleanup_tmp = || {
        let _ = fs::remove_file(&plan.tmp);
    };

    let out = Command::new(plan.backend.name())
        .args(&plan.args)
        .output()?;
    let acceptable = match plan.backend {
        FixBackend::Mkvmerge => mkvmerge_ok(&out.status),
        FixBackend::Ffmpeg => out.status.success(),
    };
    if !acceptable {
        cleanup_tmp();
        bail!(
            "{} failed with status {:?}: {}{}",
            plan.backend.name(),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // Sanity-check the remux before touching the original
    let verified = fs::metadata(&plan.tmp)
        .map_err(Into::into)
        .and_then(|m| probe_mediainfo(&plan.tmp, &m).map(|info| (m.len(), info)));
    let new_size = match verified {
        Ok((len, info))
            if info.duration.abs_diff(plan.duration) <= std::time::Duration::from_secs(2) =>
        {
            len
        }
        Ok((_, info)) => {
            cleanup_tmp();
            bail!(
                "remux output duration {:?} differs from original {:?}, keeping original",
                info.duration,
                plan.duration
            );
        }
        Err(e) => {
            cleanup_tmp();
            return Err(e.wrap_err("failed to verify remux output, keeping original"));
        }
    };

    let orig_size = fs::metadata(&plan.path)?.len();
    match mode {
        ApplyMode::Backup => {
            fs::rename(&plan.path, &bak)?;
            if let Err(e) = fs::rename(&plan.tmp, &plan.path) {
                // Try to put the original back so the library stays intact
                let _ = fs::rename(&bak, &plan.path);
                cleanup_tmp();
                return Err(e).context("failed to move fixed file into place");
            }
        }
        ApplyMode::Replace => {
            fs::rename(&plan.tmp, &plan.path).context("failed to move fixed file into place")?;
        }
    }

    Ok(FixOutcome::Fixed(orig_size as i64 - new_size as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const BACKENDS: [FixBackend; 2] = [FixBackend::Mkvmerge, FixBackend::Ffmpeg];

    fn track(id: u64, lang: &str) -> TrackRef {
        (id, Some(lang.to_owned()))
    }

    #[test]
    fn nothing_to_strip_when_every_language_is_accepted() {
        let audio = [track(0, "eng"), track(1, "ger")];
        let subs = [track(2, "en")];
        assert!(keep_lists(&audio, &subs, &mut vec![]).is_none());
    }

    #[test]
    fn both_lists_empty_is_a_no_op() {
        assert!(keep_lists(&[], &[], &mut vec![]).is_none());
    }

    #[test]
    fn undesired_audio_is_dropped_and_the_rest_kept() {
        let audio = [track(0, "eng"), track(1, "fre"), track(2, "jpn")];
        let (keep_audio, keep_subs) = keep_lists(&audio, &[], &mut vec![]).unwrap();
        assert_eq!(keep_audio, vec![0, 2]);
        assert!(keep_subs.is_empty());
    }

    /// Stripping every audio track would ruin the file
    #[test]
    fn all_audio_undesired_keeps_everything_and_notes_why() {
        let audio = [track(0, "fre"), track(1, "spa")];
        let mut notes = vec![];
        // Subtitles must still be strippable, which is why this is not a plain None
        let subs = [track(2, "eng"), track(3, "fre")];
        let (keep_audio, keep_subs) = keep_lists(&audio, &subs, &mut notes).unwrap();
        assert_eq!(keep_audio, vec![0, 1]);
        assert_eq!(keep_subs, vec![2]);
        assert_eq!(notes.len(), 1, "the guard must explain itself: {notes:?}");
    }

    #[test]
    fn all_subtitles_undesired_yields_an_empty_keep_list() {
        let audio = [track(0, "eng")];
        let subs = [track(1, "fre"), track(2, "spa")];
        let (keep_audio, keep_subs) = keep_lists(&audio, &subs, &mut vec![]).unwrap();
        assert_eq!(keep_audio, vec![0]);
        assert!(keep_subs.is_empty());
    }

    #[test]
    fn subtitles_can_be_stripped_from_a_file_with_no_audio() {
        let subs = [track(0, "fre")];
        let (keep_audio, keep_subs) = keep_lists(&[], &subs, &mut vec![]).unwrap();
        assert!(keep_audio.is_empty());
        assert!(keep_subs.is_empty());
    }

    #[test]
    fn untagged_tracks_always_survive() {
        let audio = [(0, None), track(1, "fre"), (2, Some("und".to_owned()))];
        let (keep_audio, _) = keep_lists(&audio, &[], &mut vec![]).unwrap();
        assert_eq!(keep_audio, vec![0, 2]);
    }

    #[test]
    fn region_tagged_english_is_not_undesired() {
        let audio = [track(0, "en-US"), track(1, "jpn")];
        assert!(
            keep_lists(&audio, &[], &mut vec![]).is_none(),
            "en-US audio must not be stripped"
        );
    }

    fn paths() -> (PathBuf, PathBuf) {
        (PathBuf::from("/m/out.mkv"), PathBuf::from("/m/in.mkv"))
    }

    #[test]
    fn mkvmerge_selects_only_the_kept_audio_ids() {
        let (tmp, src) = paths();
        let audio = [track(0, "eng"), track(1, "fre"), track(2, "spa")];
        let args = mkvmerge_args(&tmp, &src, &audio, &[], &mut vec![]).unwrap();
        assert_eq!(
            args,
            vec!["-o", "/m/out.mkv", "--audio-tracks", "0", "/m/in.mkv"]
        );
    }

    #[test]
    fn mkvmerge_drops_all_subtitles_with_a_dedicated_flag() {
        let (tmp, src) = paths();
        let audio = [track(0, "eng")];
        let subs = [track(1, "fre"), track(2, "spa")];
        let args = mkvmerge_args(&tmp, &src, &audio, &subs, &mut vec![]).unwrap();
        assert!(args.contains(&"--no-subtitles".to_owned()));
        // Audio is untouched, so it must not be constrained
        assert!(!args.contains(&"--audio-tracks".to_owned()));
    }

    #[test]
    fn mkvmerge_joins_kept_subtitle_ids() {
        let (tmp, src) = paths();
        let audio = [track(0, "eng")];
        let subs = [track(1, "eng"), track(2, "fre"), track(3, "ger")];
        let args = mkvmerge_args(&tmp, &src, &audio, &subs, &mut vec![]).unwrap();
        let i = args.iter().position(|a| a == "--subtitle-tracks").unwrap();
        assert_eq!(args[i + 1], "1,3");
    }

    #[test]
    fn ffmpeg_always_keeps_video_attachments_and_data() {
        let (tmp, src) = paths();
        let audio = [track(0, "eng"), track(1, "fre")];
        let args = ffmpeg_args(&tmp, &src, &audio, &[], &mut vec![]).unwrap();
        for expected in ["0:v", "0:t?", "0:d?"] {
            assert!(args.contains(&expected.to_owned()), "missing {expected}");
        }
        assert!(args.windows(2).any(|w| w == ["-c", "copy"]));
        assert_eq!(args.last().unwrap(), "/m/out.mkv");
    }

    #[test]
    fn ffmpeg_maps_kept_audio_by_per_type_index() {
        let (tmp, src) = paths();
        let audio = [track(0, "fre"), track(1, "eng"), track(2, "jpn")];
        let args = ffmpeg_args(&tmp, &src, &audio, &[], &mut vec![]).unwrap();
        assert!(args.contains(&"0:a:1".to_owned()));
        assert!(args.contains(&"0:a:2".to_owned()));
        assert!(!args.contains(&"0:a:0".to_owned()), "fre must be dropped");
    }

    #[test]
    fn ffmpeg_keeps_all_audio_with_a_wildcard_when_only_subtitles_are_dropped() {
        let (tmp, src) = paths();
        let audio = [track(0, "eng"), track(1, "ger")];
        let subs = [track(0, "eng"), track(1, "fre")];
        let args = ffmpeg_args(&tmp, &src, &audio, &subs, &mut vec![]).unwrap();
        assert!(args.contains(&"0:a?".to_owned()));
        assert!(!args.contains(&"0:a:0".to_owned()));
        assert!(args.contains(&"0:s:0".to_owned()));
        assert!(!args.contains(&"0:s?".to_owned()));
    }

    #[test]
    fn ffmpeg_uses_a_wildcard_when_no_subtitle_is_dropped() {
        let (tmp, src) = paths();
        let audio = [track(0, "eng"), track(1, "fre")];
        let subs = [track(0, "eng"), track(1, "ger")];
        let args = ffmpeg_args(&tmp, &src, &audio, &subs, &mut vec![]).unwrap();
        assert!(args.contains(&"0:s?".to_owned()));
        assert!(!args.contains(&"0:s:0".to_owned()));
    }

    fn props(ietf: Option<&str>, legacy: Option<&str>) -> MkvTrackProps {
        MkvTrackProps {
            language: legacy.map(str::to_owned),
            language_ietf: ietf.map(str::to_owned),
        }
    }

    #[test]
    fn ietf_tag_wins_and_is_passed_through_unnormalized() {
        assert_eq!(
            props(Some("de-AT"), Some("ger")).lang().as_deref(),
            Some("de-AT")
        );
    }

    #[test]
    fn undefined_ietf_tag_falls_back_to_the_legacy_code() {
        assert_eq!(
            props(Some("und"), Some("eng")).lang().as_deref(),
            Some("eng")
        );
        assert_eq!(props(Some(""), Some("eng")).lang().as_deref(), Some("eng"));
        assert_eq!(props(None, Some("eng")).lang().as_deref(), Some("eng"));
    }

    #[test]
    fn absent_language_properties_yield_none() {
        assert_eq!(props(None, None).lang(), None);
    }

    #[test]
    fn tmp_path_appends_rather_than_replaces_the_extension() {
        assert_eq!(
            tmp_path(Path::new("/m/a.b.mkv")).unwrap(),
            PathBuf::from("/m/a.b.mkv.jwatch-tmp.mkv")
        );
    }

    #[test]
    fn tmp_path_rejects_a_path_without_a_filename() {
        assert!(tmp_path(Path::new("/")).is_err());
    }

    #[test]
    fn mkvmerge_warnings_are_not_failures() {
        let status = |code: i32| {
            Command::new("sh")
                .arg("-c")
                .arg(format!("exit {code}"))
                .status()
                .expect("failed to run sh")
        };
        assert!(mkvmerge_ok(&status(0)));
        assert!(
            mkvmerge_ok(&status(1)),
            "exit 1 means warnings, not failure"
        );
        assert!(!mkvmerge_ok(&status(2)), "exit 2 is a real error");
    }

    fn require_tool(prog: &str, version_arg: &str) {
        let found = Command::new(prog)
            .arg(version_arg)
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(
            found,
            "{prog} is required to run the test suite but was not found in PATH"
        );
    }

    fn require_backend(backend: FixBackend) {
        assert!(
            backend.is_available(),
            "{} is required to run the test suite but was not found in PATH",
            backend.name()
        );
    }

    fn fixture(dir: &Path, name: &str, audio_langs: &[&str]) -> PathBuf {
        fixture_with_subs(dir, name, audio_langs, &[])
    }

    /// Builds a small tagged mkv. An empty language leaves that track untagged.
    fn fixture_with_subs(
        dir: &Path,
        name: &str,
        audio_langs: &[&str],
        sub_langs: &[&str],
    ) -> PathBuf {
        require_tool("ffmpeg", "-version");
        let path = dir.join(name);
        let srt = dir.join(format!("{name}.srt"));
        if !sub_langs.is_empty() {
            fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nx\n").unwrap();
        }

        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        cmd.args(["-f", "lavfi", "-i", "testsrc=d=2:s=64x64"]);
        for _ in audio_langs {
            cmd.args(["-f", "lavfi", "-i", "sine=d=2"]);
        }
        for _ in sub_langs {
            cmd.arg("-i").arg(&srt);
        }
        cmd.args(["-map", "0:v"]);
        for i in 1..=audio_langs.len() {
            cmd.args(["-map", &format!("{i}:a")]);
        }
        for i in 0..sub_langs.len() {
            cmd.args(["-map", &format!("{}:s", 1 + audio_langs.len() + i)]);
        }
        cmd.args(["-c:v", "libx264", "-c:a", "aac", "-c:s", "srt"]);
        for (i, lang) in audio_langs.iter().enumerate() {
            if !lang.is_empty() {
                cmd.args([&format!("-metadata:s:a:{i}"), &format!("language={lang}")]);
            }
        }
        for (i, lang) in sub_langs.iter().enumerate() {
            if !lang.is_empty() {
                cmd.args([&format!("-metadata:s:s:{i}"), &format!("language={lang}")]);
            }
        }
        cmd.args(["-shortest"]).arg(&path);

        let out = cmd.output().expect("failed to run ffmpeg");
        assert!(
            out.status.success(),
            "fixture generation failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = fs::remove_file(&srt);
        path
    }

    fn probe(path: &Path) -> MediaInfo {
        require_tool("mediainfo", "--Version");
        let metadata = fs::metadata(path).unwrap();
        probe_mediainfo(path, &metadata).unwrap()
    }

    fn langs_of(path: &Path, kind: TrackKind) -> Vec<String> {
        probe_track_layout(path)
            .unwrap()
            .into_iter()
            .filter(|t| t.kind == kind)
            .map(|t| t.language.unwrap_or_else(|| "und".to_owned()))
            .collect()
    }

    fn audio_langs(path: &Path) -> Vec<String> {
        langs_of(path, TrackKind::Audio)
    }

    fn sub_langs(path: &Path) -> Vec<String> {
        langs_of(path, TrackKind::Text)
    }

    #[test]
    fn region_tagged_english_survives_both_backends() {
        for backend in BACKENDS {
            require_backend(backend);
            let dir = tempfile::tempdir().unwrap();
            let src = fixture(dir.path(), "movie.mkv", &["en-US", "jpn"]);
            let info = probe(&src);
            assert!(
                plan_fix(&src, &info, backend).unwrap().is_none(),
                "{} planned a strip on an all-accepted file",
                backend.name()
            );
        }
    }

    #[test]
    fn backup_mode_keeps_the_original_and_strips_the_undesired_track() {
        for backend in BACKENDS {
            require_backend(backend);
            let dir = tempfile::tempdir().unwrap();
            let src = fixture(dir.path(), "movie.mkv", &["eng", "fre"]);
            let info = probe(&src);
            let plan = plan_fix(&src, &info, backend)
                .unwrap()
                .expect("expected a plan");

            apply_fix(&plan, ApplyMode::Backup).unwrap();

            let bak = dir.path().join("movie.mkv.jwatch-bak");
            assert!(bak.exists(), "{}: backup missing", backend.name());
            assert!(!plan.tmp.exists(), "{}: tmp left behind", backend.name());
            assert_eq!(audio_langs(&src), ["en"], "{}", backend.name());
            assert_eq!(
                audio_langs(&bak),
                ["en", "fr"],
                "{}: backup was modified",
                backend.name()
            );
        }
    }

    #[test]
    fn replace_mode_leaves_no_backup() {
        for backend in BACKENDS {
            require_backend(backend);
            let dir = tempfile::tempdir().unwrap();
            let src = fixture(dir.path(), "movie.mkv", &["eng", "fre"]);
            let info = probe(&src);
            let plan = plan_fix(&src, &info, backend)
                .unwrap()
                .expect("expected a plan");

            apply_fix(&plan, ApplyMode::Replace).unwrap();

            assert!(!dir.path().join("movie.mkv.jwatch-bak").exists());
            assert!(!plan.tmp.exists());
            assert_eq!(audio_langs(&src), ["en"], "{}", backend.name());
        }
    }

    #[test]
    fn untagged_audio_is_never_stripped_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let src = fixture(dir.path(), "movie.mkv", &["", "fre", "eng"]);
        let info = probe(&src);
        let plan = plan_fix(&src, &info, FixBackend::Ffmpeg)
            .unwrap()
            .expect("expected a plan");
        apply_fix(&plan, ApplyMode::Replace).unwrap();
        assert_eq!(audio_langs(&src), ["und", "en"]);
    }

    #[test]
    fn a_duration_mismatch_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let src = fixture(dir.path(), "movie.mkv", &["eng", "fre"]);
        let info = probe(&src);
        let mut plan = plan_fix(&src, &info, FixBackend::Ffmpeg)
            .unwrap()
            .expect("expected a plan");
        // Pretend the source was far longer than it is, so verification must reject
        plan.duration = Duration::from_secs(9999);
        let before = fs::read(&src).unwrap();

        let err = apply_fix(&plan, ApplyMode::Replace).unwrap_err();

        assert!(
            err.to_string().contains("duration"),
            "unexpected error: {err:?}"
        );
        assert_eq!(fs::read(&src).unwrap(), before, "original was modified");
        assert!(!plan.tmp.exists(), "tmp left behind");
    }

    #[test]
    fn a_failed_remux_leaves_no_tmp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("not-a-video.mkv");
        fs::write(&bogus, b"definitely not matroska").unwrap();
        let tmp = tmp_path(&bogus).unwrap();
        let plan = FixPlan {
            path: bogus.clone(),
            tmp: tmp.clone(),
            backend: FixBackend::Ffmpeg,
            args: vec![
                "-hide_banner".to_owned(),
                "-loglevel".to_owned(),
                "error".to_owned(),
                "-y".to_owned(),
                "-i".to_owned(),
                bogus.to_string_lossy().into_owned(),
                "-c".to_owned(),
                "copy".to_owned(),
                tmp.to_string_lossy().into_owned(),
            ],
            duration: Duration::from_secs(1),
            notes: vec![],
        };

        assert!(apply_fix(&plan, ApplyMode::Replace).is_err());
        assert!(!tmp.exists(), "tmp left behind after a failed remux");
        assert!(bogus.exists(), "original was removed");
    }

    /// Only subtitles dropped, so audio is kept via the wildcard rather than enumerated
    #[test]
    fn subtitle_only_strip_survives_both_backends() {
        for backend in BACKENDS {
            require_backend(backend);
            let dir = tempfile::tempdir().unwrap();
            let src = fixture_with_subs(
                dir.path(),
                "movie.mkv",
                &["eng", "ger"],
                &["eng", "fre", "spa"],
            );
            let info = probe(&src);
            let plan = plan_fix(&src, &info, backend)
                .unwrap()
                .expect("expected a plan");

            let before = fs::metadata(&src).unwrap().len();
            let outcome = apply_fix(&plan, ApplyMode::Replace).unwrap();
            let after = fs::metadata(&src).unwrap().len();

            assert_eq!(audio_langs(&src), ["en", "de"], "{}", backend.name());
            assert_eq!(sub_langs(&src), ["en"], "{}", backend.name());
            // Reported delta must match reality, including a remux that grew the file
            assert_eq!(
                outcome,
                FixOutcome::Fixed(before as i64 - after as i64),
                "{}",
                backend.name()
            );
        }
    }

    #[test]
    fn an_existing_temp_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let src = fixture(dir.path(), "movie.mkv", &["eng", "fre"]);
        let info = probe(&src);
        let plan = plan_fix(&src, &info, FixBackend::Ffmpeg)
            .unwrap()
            .expect("expected a plan");
        fs::write(&plan.tmp, b"leftover from an interrupted run").unwrap();
        let src_before = fs::read(&src).unwrap();

        let outcome = apply_fix(&plan, ApplyMode::Replace).unwrap();

        assert_eq!(outcome, FixOutcome::SkippedTempExists(plan.tmp.clone()));
        assert_eq!(
            fs::read(&plan.tmp).unwrap(),
            b"leftover from an interrupted run"
        );
        assert_eq!(fs::read(&src).unwrap(), src_before);
    }

    /// Second `--apply backup` pass over a file that still has undesired tracks, e.g. after
    /// the accepted-language list was tightened. Without the guard the rename promotes the
    /// already-stripped file over the true original.
    #[test]
    fn a_second_backup_run_does_not_overwrite_the_first_backup() {
        let dir = tempfile::tempdir().unwrap();
        let src = fixture(dir.path(), "movie.mkv", &["eng", "fre"]);
        let src_before = fs::read(&src).unwrap();
        let bak = dir.path().join("movie.mkv.jwatch-bak");
        fs::write(&bak, b"the untouched original from the first pass").unwrap();

        let info = probe(&src);
        let plan = plan_fix(&src, &info, FixBackend::Ffmpeg).unwrap().unwrap();
        let outcome = apply_fix(&plan, ApplyMode::Backup).unwrap();

        assert_eq!(outcome, FixOutcome::SkippedBackupExists(bak.clone()));
        assert_eq!(
            fs::read(&bak).unwrap(),
            b"the untouched original from the first pass",
            "the first backup was replaced"
        );
        assert_eq!(fs::read(&src).unwrap(), src_before);
        assert!(!plan.tmp.exists(), "no remux should have run");
    }

    /// Replace writes no backup, so an existing one must not block it
    #[test]
    fn replace_mode_ignores_an_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let src = fixture(dir.path(), "movie.mkv", &["eng", "fre"]);
        fs::write(dir.path().join("movie.mkv.jwatch-bak"), b"old backup").unwrap();

        let info = probe(&src);
        let plan = plan_fix(&src, &info, FixBackend::Ffmpeg).unwrap().unwrap();
        let outcome = apply_fix(&plan, ApplyMode::Replace).unwrap();

        assert!(matches!(outcome, FixOutcome::Fixed(_)));
        assert_eq!(audio_langs(&src), ["en"]);
    }

    #[test]
    fn quoted_arguments_round_trip_through_a_shell() {
        for arg in [
            "plain.mkv",
            "/m/a b.mkv",
            "it's.mkv",
            "$(rm -rf /).mkv",
            "back`tick`.mkv",
            "semi;colon.mkv",
            "new\nline.mkv",
            "",
        ] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(arg)))
                .output()
                .expect("failed to run sh");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                arg,
                "{arg:?} did not round-trip as {}",
                shell_quote(arg)
            );
        }
    }

    #[test]
    fn hardlinked_files_report_every_link() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mkv");
        let b = dir.path().join("b.mkv");
        fs::write(&a, b"x").unwrap();
        fs::hard_link(&a, &b).unwrap();

        assert_eq!(hardlinks(&a), 2);
        assert_eq!(hardlinks(&b), 2);
    }

    #[test]
    fn unshared_and_missing_files_report_one_link() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.mkv");
        fs::write(&plain, b"x").unwrap();

        assert_eq!(hardlinks(&plain), 1);
        assert_eq!(hardlinks(&dir.path().join("does-not-exist.mkv")), 1);
    }

    #[test]
    fn ordinary_paths_are_left_unquoted() {
        assert_eq!(shell_quote("/media/movie.mkv"), "/media/movie.mkv");
        assert_eq!(shell_quote("--audio-tracks"), "--audio-tracks");
    }

    #[test]
    fn apply_mode_parses_from_the_command_line() {
        use argh::FromArgValue;
        assert_eq!(
            ApplyMode::from_arg_value("backup").unwrap(),
            ApplyMode::Backup
        );
        assert_eq!(
            ApplyMode::from_arg_value("REPLACE").unwrap(),
            ApplyMode::Replace
        );
        assert!(ApplyMode::from_arg_value("clobber").is_err());
    }
}
