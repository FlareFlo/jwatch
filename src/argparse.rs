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
}
