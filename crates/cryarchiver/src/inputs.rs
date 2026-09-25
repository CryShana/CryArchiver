use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cryarchiver_engine::naming::is_cli_output;
use cryarchiver_engine::text::display_path;
use globset::{GlobBuilder, escape};

#[derive(Debug, Default)]
pub struct Inputs {
	pub files: Vec<PathBuf>,
	pub notes: Vec<String>,
	pub problems: usize,
}

impl Inputs {
	fn problem(&mut self, note: String) {
		self.notes.push(note);
		self.problems += 1;
	}
}

pub fn expand(args: &[PathBuf]) -> Inputs {
	let mut inputs = Inputs::default();

	for arg in args {
		if arg.is_file() {
			inputs.files.push(arg.clone());
		} else if arg.is_dir() {
			match list_files(arg) {
				Ok(files) => {
					inputs
						.files
						.extend(files.into_iter().filter(|file| !is_cli_output(file)));
				}
				Err(err) => inputs.problem(format!("{}: cannot read the folder: {err}", display_path(arg))),
			}
		} else if has_wildcard(arg) {
			expand_wildcard(&mut inputs, arg);
		} else {
			inputs.problem(format!("{}: not found", display_path(arg)));
		}
	}

	inputs.files.sort();
	inputs.files.dedup();
	inputs
}

fn has_wildcard(path: &Path) -> bool {
	path.to_string_lossy().contains(['*', '?'])
}

fn escape_pattern_keeping_wildcards(pattern: &str) -> String {
	let mut result = String::new();
	let mut current = String::new();

	for c in pattern.chars() {
		if c == '*' || c == '?' {
			if !current.is_empty() {
				result.push_str(&escape(&current));
				current.clear();
			}
			result.push(c);
		} else {
			current.push(c);
		}
	}

	if !current.is_empty() {
		result.push_str(&escape(&current));
	}

	result
}

fn expand_wildcard(inputs: &mut Inputs, arg: &Path) {
	let shown = display_path(arg);
	let parent = arg.parent().unwrap_or(Path::new(""));
	if has_wildcard(parent) && !parent.is_dir() {
		inputs.problem(format!("{shown}: wildcards work only in the file name"));
		return;
	}

	let Some(pattern) = arg.file_name().and_then(|name| name.to_str()) else {
		inputs.problem(format!("{shown}: the pattern is not valid text"));
		return;
	};

	let escaped_pattern = escape_pattern_keeping_wildcards(pattern);
	let Ok(glob) = GlobBuilder::new(&escaped_pattern)
		.case_insensitive(cfg!(windows))
		.literal_separator(true)
		.build()
	else {
		inputs.problem(format!("{shown}: invalid wildcard pattern"));
		return;
	};
	let matcher = glob.compile_matcher();

	let before = inputs.files.len();
	match list_files(parent) {
		Ok(files) => {
			inputs.files.extend(files.into_iter().filter(|file| {
				file.file_name()
					.is_some_and(|name| matcher.is_match(name) && !is_cli_output(file))
			}));
		}
		Err(err) => {
			inputs.problem(format!("{shown}: cannot read the folder: {err}"));
			return;
		}
	}

	if inputs.files.len() == before {
		inputs.notes.push(format!("{shown}: no files match"));
	}
}

fn list_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
	let read_from = if dir.as_os_str().is_empty() {
		Path::new(".")
	} else {
		dir
	};
	let entries = fs::read_dir(read_from)?;

	Ok(entries
		.flatten()
		.map(|entry| dir.join(entry.file_name()))
		.filter(|path| path.is_file())
		.collect())
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	fn touch(path: &Path) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, b"x").unwrap();
	}

	#[test]
	fn folders_expand_one_level_and_skip_cli_outputs() {
		let dir = tempfile::tempdir().unwrap();
		for name in ["b.mp3", "a.flac", "a.cry.opus", "sub/c.flac"] {
			touch(&dir.path().join(name));
		}

		let inputs = expand(&[dir.path().to_path_buf()]);

		assert_eq!(inputs.files, vec![dir.path().join("a.flac"), dir.path().join("b.mp3")]);
		assert!(inputs.notes.is_empty());
		assert_eq!(inputs.problems, 0);
	}

	#[test]
	fn wildcards_match_file_names_only() {
		let dir = tempfile::tempdir().unwrap();
		for name in ["a.flac", "b.flac", "c.mp3", "b.cry.flac"] {
			touch(&dir.path().join(name));
		}

		let inputs = expand(&[dir.path().join("*.flac")]);

		assert_eq!(inputs.files, vec![dir.path().join("a.flac"), dir.path().join("b.flac")]);
	}

	#[test]
	fn explicit_files_are_kept_and_problems_become_notes() {
		let dir = tempfile::tempdir().unwrap();
		let explicit = dir.path().join("old.cry.opus");
		touch(&explicit);

		let inputs = expand(&[
			explicit.clone(),
			explicit.clone(),
			dir.path().join("missing.flac"),
			dir.path().join("*").join("*.flac"),
			dir.path().join("*.wav"),
		]);

		assert_eq!(inputs.files, vec![explicit]);
		assert_eq!(inputs.notes.len(), 3);
		assert!(inputs.notes[0].contains("not found"));
		assert!(inputs.notes[1].contains("only in the file name"));
		assert!(inputs.notes[2].contains("no files match"));
		assert_eq!(inputs.problems, 2);
	}

	#[test]
	fn bracketed_folders_are_read_literally() {
		let dir = tempfile::tempdir().unwrap();
		for name in ["Album [2020]/a.flac", "Album [2020]/b.mp3"] {
			touch(&dir.path().join(name));
		}

		let inputs = expand(&[dir.path().join("Album [2020]").join("*.flac")]);

		assert_eq!(inputs.files, vec![dir.path().join("Album [2020]").join("a.flac")]);
		assert!(inputs.notes.is_empty());
	}

	#[test]
	fn brackets_are_literal_in_patterns() {
		let dir = tempfile::tempdir().unwrap();
		for name in ["song [live].flac", "song l.flac", "other.flac"] {
			touch(&dir.path().join(name));
		}

		let inputs = expand(&[dir.path().join("*[live]*.flac")]);

		assert_eq!(inputs.files, vec![dir.path().join("song [live].flac")]);
		assert!(inputs.notes.is_empty());
	}

	#[test]
	fn missing_bracketed_file_is_not_found() {
		let dir = tempfile::tempdir().unwrap();

		let inputs = expand(&[dir.path().join("missing [live].flac")]);

		assert!(inputs.files.is_empty());
		assert_eq!(inputs.notes.len(), 1);
		assert!(inputs.notes[0].contains("not found"));
		assert_eq!(inputs.problems, 1);
	}

	#[test]
	#[cfg(windows)]
	fn wildcards_ignore_case_on_windows() {
		let dir = tempfile::tempdir().unwrap();
		touch(&dir.path().join("a.flac"));

		let inputs = expand(&[dir.path().join("*.FLAC")]);

		assert_eq!(inputs.files, vec![dir.path().join("a.flac")]);
		assert!(inputs.notes.is_empty());
	}
}
