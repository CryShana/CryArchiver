use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const CLI_MARKER: &str = "cry";

pub fn cli_output_dir(source: &Path, output_dir: Option<&Path>) -> PathBuf {
	match output_dir {
		Some(dir) => dir.to_path_buf(),
		None => source.parent().map(Path::to_path_buf).unwrap_or_default(),
	}
}

pub fn cli_output_path(source: &Path, output_dir: Option<&Path>, extension: impl AsRef<OsStr>) -> PathBuf {
	let mut name = OsString::from(source.file_stem().unwrap_or_default());
	name.push(format!(".{CLI_MARKER}."));
	name.push(extension);

	cli_output_dir(source, output_dir).join(name)
}

pub fn is_cli_output(path: &Path) -> bool {
	path.file_stem()
		.map(Path::new)
		.and_then(Path::extension)
		.is_some_and(|marker| marker == CLI_MARKER)
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::*;

	#[test]
	fn outputs_go_next_to_the_source_or_into_the_output_dir() {
		assert_eq!(
			cli_output_path(Path::new("music/01.テスト.mp3"), None, "opus"),
			Path::new("music/01.テスト.cry.opus")
		);
		assert_eq!(
			cli_output_path(Path::new("music/a.flac"), Some(Path::new("out")), "opus"),
			Path::new("out/a.cry.opus")
		);
		assert_eq!(cli_output_dir(Path::new("a.flac"), None), Path::new(""));
	}

	#[test]
	fn recognizes_cli_outputs() {
		assert!(is_cli_output(Path::new("a/video.cry.mkv")));
		assert!(!is_cli_output(Path::new("a/cry.mkv")));
		assert!(!is_cli_output(Path::new("a/video.mkv")));
	}
}
