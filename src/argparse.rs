use std::path::PathBuf;

#[derive(argh::FromArgs, Debug)]
/// WIP
pub struct Args {
	#[argh(positional)]
	/// path to folder which gets parsed
	pub path: String,
}