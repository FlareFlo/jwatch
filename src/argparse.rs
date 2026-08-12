use crate::fix::ApplyMode;

#[derive(argh::FromArgs, Debug)]
/// WIP
pub struct Args {
    #[argh(positional)]
    /// path to folder which gets parsed
    pub path: String,

    #[argh(option)]
    /// path to cache database
    pub db_path: Option<String>,

    #[argh(option, short = 'j', default = "1")]
    /// number of parallel mediainfo probes
    pub jobs: usize,

    #[argh(switch)]
    /// remux mkv files to strip audio/subtitle tracks in undesired languages.
    /// Prints the planned commands; nothing is modified unless --apply is given
    pub fix: bool,

    #[argh(option)]
    /// how to apply the fixes planned by --fix: "backup" keeps each original as
    /// <name>.jwatch-bak, "replace" overwrites it
    pub apply: Option<ApplyMode>,

    #[argh(switch)]
    /// force preview mode, overriding --apply
    pub dry_run: bool,
}
